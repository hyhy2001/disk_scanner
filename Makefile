.PHONY: all build test clean clean-env static-build install setup-env require-cargo require-zigbuild help

# ─── Project-local, self-contained Rust toolchain ───────────────────────
# `make setup-env` installs rustup + cargo into ./.rust (NOT ~/.cargo), so the
# whole build is hermetic: toolchain, registry cache, and git deps all live
# under the project dir and never touch the system install. Every target below
# runs through $(CARGO), which points at that local toolchain.
LOCAL_RUST         := $(CURDIR)/.rust
export CARGO_HOME  := $(LOCAL_RUST)/cargo
export RUSTUP_HOME := $(LOCAL_RUST)/rustup
RUST_VERSION       ?= stable

# Prefer the project-local cargo; fall back to a system cargo only if
# setup-env has not been run yet (keeps `make build` working for people who
# already have a global toolchain).
CARGO := $(shell if [ -x "$(CARGO_HOME)/bin/cargo" ]; then \
	echo "$(CARGO_HOME)/bin/cargo"; else command -v cargo; fi)

RELEASE_BIN = target/release/duscan

# ─── Cross-target OS builds ─────────────────────────────────────────────
# `make build OS=RHEL8` produces a binary that runs on RHEL8 (glibc 2.28).
# The build host here ships a newer glibc, so a plain `make build` links
# against symbols (up to GLIBC_2.39) that do not exist on RHEL8. We use
# cargo-zigbuild to target glibc 2.28 explicitly: this keeps a real glibc
# build (so the scanner's `statx` NFS hot-path still compiles, unlike musl)
# while capping the required glibc at RHEL8's version.
OS ?=
ifeq ($(OS),RHEL8)
RHEL8_TARGET     := x86_64-unknown-linux-gnu.2.28
RHEL8_TARGET_DIR := x86_64-unknown-linux-gnu
RHEL8_BIN        := target/$(RHEL8_TARGET_DIR)/release/duscan
endif

all: build

# Download and install a project-local Rust toolchain into ./.rust.
setup-env:
	@echo "Installing a project-local Rust toolchain into $(LOCAL_RUST) ..."
	@mkdir -p "$(LOCAL_RUST)"
	@if [ -x "$(CARGO_HOME)/bin/cargo" ]; then \
		echo "Local toolchain already present: $(CARGO_HOME)/bin/cargo"; \
	else \
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
			| sh -s -- -y --no-modify-path --profile minimal \
				--default-toolchain $(RUST_VERSION); \
	fi
	@"$(CARGO_HOME)/bin/cargo" --version
	@echo ""
	@echo "Done. This Makefile already runs cargo from the local toolchain."
	@echo "To use it directly in your shell:"
	@echo "  export CARGO_HOME=$(CARGO_HOME)"
	@echo "  export RUSTUP_HOME=$(RUSTUP_HOME)"
	@echo "  export PATH=$(CARGO_HOME)/bin:\$$PATH"

# Guard: fail with a helpful message when no cargo is available at all.
require-cargo:
	@if [ -z "$(CARGO)" ]; then \
		echo "No cargo found. Run 'make setup-env' first to install a local toolchain."; \
		exit 1; \
	fi

ifeq ($(OS),RHEL8)
build: require-cargo require-zigbuild
	$(CARGO) zigbuild --release -p duscan --target $(RHEL8_TARGET)
	cp $(RHEL8_BIN) ./duscan
	strip ./duscan
	@echo "RHEL8 binary: ./duscan ($$(ls -lh duscan | awk '{print $$5}'))"
	@echo "Max glibc required: $$(objdump -T ./duscan 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1) (RHEL8 provides 2.28)"
else
build: require-cargo
	$(CARGO) build --release -p duscan
	cp $(RELEASE_BIN) ./duscan
	strip ./duscan
	@echo "Binary: ./duscan ($$(ls -lh duscan | awk '{print $$5}'))"
endif

# Guard: cargo-zigbuild is required for the RHEL8 cross-glibc target.
require-zigbuild:
	@if ! command -v cargo-zigbuild >/dev/null 2>&1; then \
		echo "cargo-zigbuild not found. Install it with:"; \
		echo "  cargo install cargo-zigbuild   (and install 'zig' on PATH)"; \
		exit 1; \
	fi
	@if ! command -v zig >/dev/null 2>&1; then \
		echo "zig not found on PATH. cargo-zigbuild needs the zig compiler."; \
		echo "  See https://ziglang.org/download/ or 'pip install ziglang'"; \
		exit 1; \
	fi

static-build: require-cargo
	$(CARGO) rustc --release -p duscan -- -C target-feature=+crt-static
	cp target/release/duscan ./duscan-static
	strip ./duscan-static
	@echo "Static binary: ./duscan-static ($$(ls -lh duscan-static | awk '{print $$5}'))"

test: require-cargo
	$(CARGO) test -p duscan

clean: require-cargo
	$(CARGO) clean
	rm -f duscan duscan-static

# Remove the project-local toolchain (and its caches) entirely.
clean-env:
	rm -rf "$(LOCAL_RUST)"

install: build
	cp ./duscan /usr/local/bin/duscan

help:
	@echo "Targets:"
	@echo "  make setup-env    Install a project-local Rust toolchain into ./.rust"
	@echo "  make build        Release binary (dynamic), using the local toolchain"
	@echo "  make build OS=RHEL8  Release binary for RHEL8 (glibc 2.28, via cargo-zigbuild)"
	@echo "  make static-build Static binary (fully linked)"
	@echo "  make test         Run tests"
	@echo "  make clean        Clean build artifacts (cargo clean + binaries)"
	@echo "  make clean-env    Remove the project-local toolchain in ./.rust"
	@echo "  make install      Copy ./duscan to /usr/local/bin"
	@echo ""
	@echo "Using cargo: $(CARGO)"

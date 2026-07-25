.PHONY: all build test clean clean-env static-build install setup-env require-cargo help

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

build: require-cargo
	$(CARGO) build --release -p duscan
	cp $(RELEASE_BIN) ./duscan
	strip ./duscan
	@echo "Binary: ./duscan ($$(ls -lh duscan | awk '{print $$5}'))"

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
	@echo "  make static-build Static binary (fully linked)"
	@echo "  make test         Run tests"
	@echo "  make clean        Clean build artifacts (cargo clean + binaries)"
	@echo "  make clean-env    Remove the project-local toolchain in ./.rust"
	@echo "  make install      Copy ./duscan to /usr/local/bin"
	@echo ""
	@echo "Using cargo: $(CARGO)"

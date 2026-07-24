.PHONY: all build test clean static-build install help

all: build

RELEASE_BIN = target/release/duscan

build:
	cargo build --release -p duscan
	cp $(RELEASE_BIN) ./duscan
	strip ./duscan
	@echo "Binary: ./duscan ($$(ls -lh duscan | awk '{print $$5}'))"

static-build:
	cargo rustc --release -p duscan -- -C target-feature=+crt-static
	cp target/release/duscan ./duscan-static
	strip ./duscan-static
	@echo "Static binary: ./duscan-static ($$(ls -lh duscan-static | awk '{print $$5}'))"

test:
	cargo test -p duscan

clean:
	cargo clean
	rm -f duscan duscan-static

install: build
	cp ./duscan /usr/local/bin/duscan

help:
	@echo "Targets:"
	@echo "  make build        Release binary (dynamic)"
	@echo "  make static-build Static binary (fully linked)"
	@echo "  make test         Run tests"
	@echo "  make clean        Clean artifacts"
	@echo "  make install      Copy to /usr/local/bin"

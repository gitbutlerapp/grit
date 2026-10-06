# Convenience wrapper around the cargo/test workflows documented in AGENTS.md.
#
# Targets:
#   make build        - release build of the grit-git CLI
#   make debug        - debug build of the grit-git CLI
#   make test         - run the Rust unit/integration tests
#   make clippy       - lint all crates
#   make fmt          - format all crates
#   make clean        - remove build artifacts

CARGO ?= cargo

.PHONY: all build debug test clippy fmt clean

all: build

build:
	$(CARGO) build --release -p grit-git

debug:
	$(CARGO) build -p grit-git

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --workspace --all-targets

fmt:
	$(CARGO) fmt --all

clean:
	$(CARGO) clean

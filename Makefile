# Convenience wrapper around the cargo/test workflows documented in AGENTS.md.
#
# Targets:
#   make build        - release build of the grit CLI
#   make debug        - debug build of the grit CLI
#   make test         - run the Rust unit/integration tests
#   make clippy       - lint all crates (warnings fail CI)
#   make fmt          - format all crates
#   make gate         - pre-integration gate (fmt, clippy, workspace tests)
#   make clean        - remove build artifacts

CARGO ?= cargo
CARGO_BUILD_JOBS ?= $(shell nproc 2>/dev/null || echo 2)
export CARGO_BUILD_JOBS

.PHONY: all build debug test clippy fmt gate clean

all: build

build:
	$(CARGO) build --release -p grit-cli

debug:
	$(CARGO) build -p grit-cli

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --workspace -- -D warnings

fmt:
	$(CARGO) fmt --all

gate:
	./scripts/gate.sh

clean:
	$(CARGO) clean

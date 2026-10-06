# Convenience wrapper around the cargo/test workflows documented in AGENTS.md.
#
# Targets:
#   make build        - release build of the grit-git CLI
#   make debug        - debug build of the grit-git CLI
#   make test         - run the Rust unit/integration tests
#   make clippy       - lint all crates (warnings fail CI)
#   make ci           - fmt check, clippy -D warnings, unit tests, strict smoke
#   make fmt          - format all crates
#   make clean        - remove build artifacts

CARGO ?= cargo
CARGO_BUILD_JOBS ?= $(shell nproc 2>/dev/null || echo 2)
export CARGO_BUILD_JOBS

SMOKE_LIST := data/ci/smoke-tests.txt

.PHONY: all build debug test clippy fmt ci smoke clean

all: build

build:
	$(CARGO) build --release -p grit-git

debug:
	$(CARGO) build -p grit-git

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --workspace -- -D warnings

ci: fmt-check clippy test smoke

fmt-check:
	$(CARGO) fmt --all --check

smoke: build
	@dir="$${SMOKE_DATA_DIR:-$$(mktemp -d)}"; \
	./scripts/run-tests.sh --strict --quiet --no-catalog \
		--list $(SMOKE_LIST) \
		--data-dir "$$dir"

fmt:
	$(CARGO) fmt --all

clean:
	$(CARGO) clean

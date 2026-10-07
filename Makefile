# Convenience wrapper around the cargo/test workflows documented in AGENTS.md.
#
# Targets:
#   make build        - release build of the grit CLI
#   make debug        - debug build of the grit CLI
#   make test         - run the Rust unit/integration tests
#   make clippy       - lint all crates (warnings fail CI)
#   make fmt          - format all crates
#   make doc          - build API docs (rustdoc warnings fail)
#   make docs         - build the static documentation site
#   make docs-check   - verify site output and external links
#   make gate         - pre-integration gate (fmt, clippy, rustdoc, workspace tests)
#   make clean        - remove build artifacts

CARGO ?= cargo
CARGO_BUILD_JOBS ?= $(shell nproc 2>/dev/null || echo 2)
export CARGO_BUILD_JOBS

<<<<<<< New base: lib: rustfmt lib.rs re-exports after main rebase
.PHONY: all build debug test clippy fmt gate clean docs docs-check
||||||| Common ancestor
.PHONY: all build debug test clippy fmt gate clean
=======
.PHONY: all build debug test clippy fmt doc gate clean
>>>>>>> Current commit: docs: fix grit-lib and grit-cli rustdoc warnings

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

doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --all-features

gate:
	./scripts/gate.sh

clean:
	$(CARGO) clean

docs:
	python3 scripts/site.py

docs-check:
	python3 scripts/site.py --check
	python3 scripts/linkcheck.py

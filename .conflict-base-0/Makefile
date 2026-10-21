# Convenience wrapper around the cargo/test workflows documented in AGENTS.md.
#
# Targets:
#   make build        - release build of the grit CLI
#   make debug        - debug build of the grit CLI
#   make test         - run the Rust unit/integration tests
#   make clippy       - lint all crates (warnings fail CI)
#   make fmt          - format all crates
#   make doc          - build API docs (rustdoc warnings fail)
#   make docs         - preview grit-scm.com locally (Next.js dev server in site/)
#   make docs-check   - same checks as the Site workflow: content, links, tests, build
#   make gate         - pre-integration gate (fmt, clippy, rustdoc, workspace tests)
#   make public-api   - regenerate grit-lib/public-api.txt (pinned nightly + cargo-public-api)
#   make public-api-check - diff committed public API snapshot against a fresh run
#   make coverage     - llvm-cov on grit-lib with per-module floor ratchet
#   make clean        - remove build artifacts

CARGO ?= cargo
CARGO_BUILD_JOBS ?= $(shell nproc 2>/dev/null || echo 2)
export CARGO_BUILD_JOBS

LLVM_COV_TARGET ?= target/llvm-cov-target
GRIT_COV_BIN := $(CURDIR)/$(LLVM_COV_TARGET)/debug/grit

.PHONY: all build debug test clippy fmt doc gate clean docs docs-check coverage public-api public-api-check bench-lib bench-scenarios

BENCH_DIR := target/bench
CRITERION_DIR := target/criterion
TIER ?= M
TIER_ARG := $(shell echo $(TIER) | tr 'A-Z' 'a-z')
SMOKE ?= 0
SCENARIO_HOST := $(shell hostname -s 2>/dev/null || echo localhost)

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

bench-lib:
	@mkdir -p $(BENCH_DIR)
	$(CARGO) build --release -p grit-utils
	$(CARGO) bench -p grit-lib --features unstable-bench-helpers
	./target/release/grit-bench criterion-summary --criterion-dir $(CRITERION_DIR) --format json --output $(BENCH_DIR)/criterion-summary.json

bench-scenarios:
	@mkdir -p $(BENCH_DIR)
	$(CARGO) build --release -p grit-cli -p grit-http-server -p grit-utils
	./target/release/grit-bench scenarios --tier $(TIER_ARG) $(if $(filter 1,$(SMOKE)),--smoke,) --format json --output $(BENCH_DIR)/scenarios-$(SCENARIO_HOST).json
	./target/release/grit-bench render --format markdown --input $(BENCH_DIR)/scenarios-$(SCENARIO_HOST).json --output $(BENCH_DIR)/scenarios-$(SCENARIO_HOST).md

public-api:
	./scripts/public-api.sh generate

public-api-check:
	./scripts/public-api.sh check

clean:
	$(CARGO) clean

public-api:
	./scripts/public-api.sh generate

public-api-check:
	./scripts/public-api.sh check

site/node_modules: site/package-lock.json
	cd site && npm ci
	@touch site/node_modules

docs: site/node_modules
	$(CARGO) doc -p grit-lib --no-deps
	cd site && npm run dev

docs-check: site/node_modules
	$(CARGO) doc -p grit-lib --no-deps
	cd site && GRIT_SITE_STRICT=1 npm run check && npm test && npm run typecheck && GRIT_SITE_STRICT=1 npm run build

LLVM_COV_FEATURES ?= http-ureq,unstable-bench-helpers

coverage:
	@mkdir -p target/llvm-cov target/doc
	$(CARGO) build -p grit-cli --target-dir $(LLVM_COV_TARGET)
	$(CARGO) build -p grit-http-server --target-dir $(LLVM_COV_TARGET)
	./scripts/public-api.sh ensure-toolchain
	RUSTDOCFLAGS= python3 -c "import sys; from pathlib import Path; sys.path.insert(0,'scripts'); from public_api import parse_pins; from public_fn_coverage import ensure_rustdoc_json; ensure_rustdoc_json(nightly=parse_pins()['nightly_date'], output=Path('target/doc/grit_lib.json'))"
	GRIT_BIN=$(GRIT_COV_BIN) $(CARGO) llvm-cov -p grit-lib --features $(LLVM_COV_FEATURES) --lib --tests --json --output-path target/llvm-cov/summary.json -- --test-threads=1
	GRIT_BIN=$(GRIT_COV_BIN) cargo +nightly-2026-10-01 llvm-cov -p grit-lib --features $(LLVM_COV_FEATURES) --doc --json --output-path target/llvm-cov/functions-doc.json -- --test-threads=1
	GRIT_BIN=$(GRIT_COV_BIN) python3 scripts/coverage.py --input target/llvm-cov/summary.json
	GRIT_BIN=$(GRIT_COV_BIN) python3 scripts/public_fn_coverage.py --cov-json target/llvm-cov/summary.json --cov-json target/llvm-cov/functions-doc.json
	GRIT_BIN=$(GRIT_COV_BIN) $(CARGO) llvm-cov -p grit-lib --features $(LLVM_COV_FEATURES) report --html --output-dir target/llvm-cov/html
	GRIT_BIN=$(GRIT_COV_BIN) $(CARGO) llvm-cov -p grit-lib --features $(LLVM_COV_FEATURES) report --lcov --output-path target/llvm-cov/lcov.info

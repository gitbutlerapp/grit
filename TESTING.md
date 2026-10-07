# TESTING.md — Grit Test Strategy

## Overview

Grit is **`grit-lib`** (the library) and **`grit-cli`** (the `grit` client). Behavior is validated with **Rust tests** in the workspace crates:

1. **Unit and integration tests in `grit-lib`** — core Git semantics (objects, packs, refs, index, diff, revwalk, config, transport, serving, and related areas). Prefer typed API tests over shelling out.
2. **Coverage tests for every public library interface** — each public API surface gets explicit tests; line-coverage targets are in **ROADMAP.md**.
3. **Workspace integration tests** — `grit-lib/tests/`, `grit-cli`, and `grit-examples` for end-to-end flows (temp repos, `file://`, local smart HTTP).
4. **Scope guards** — `grit-lib/tests/pruned_modules.rs` fails if removed grit-git-only module files or `mod` declarations reappear under `grit-lib/src` (see `docs/v1-scope.md`).

Compatibility with Git's **on-disk formats and wire protocols** is checked inside those tests against the system **`git`** binary: tests build repositories with `git`, read them with `grit-lib` (and the reverse), run `git fsck` on what `grit` writes, and push/fetch between the two. There is no ported upstream shell harness and no vendored Git source tree; Git's command-line text, flags and exit codes are not a compatibility target.

## Running tests

```bash
# Library (required before every commit)
cargo test -p grit-lib --lib

# CI unit-test job (grit-lib + grit-cli)
cargo test -p grit-lib -p grit-cli

# Broader workspace
cargo test --workspace

# Release build sanity (optional)
cargo test -p grit-lib --lib --release
```

### Transport tests (fetch and push over smart HTTP)

The smart-HTTP tests in `grit-lib/tests/` (`transport_http*`, `matrix_fetch`, `matrix_push`, `matrix_sha256`, `matrix_credentials`) start `grit-http-server`, which serves repositories by running **`grit upload-pack`** and **`grit receive-pack`**. The library client fetches and pushes against it, and `git` checks the results. They need the `http-ureq` feature and both binaries built first; without the binaries they print `SKIP:` and pass, so build before running:

```bash
cargo build -p grit-cli -p grit-http-server
cargo test -p grit-lib --features http-ureq \
  --test transport_http --test transport_http_auth \
  --test transport_http_proxy_cookies --test transport_http_redirect_auth \
  --test matrix_fetch --test matrix_push --test matrix_sha256 --test matrix_credentials
```

Add `-- --nocapture` and look for `SKIP:` lines to confirm nothing was skipped.

## Lint, format, and pre-integration gate

The repo root [`rust-toolchain.toml`](rust-toolchain.toml) pins **Rust 1.99.0** with **`rustfmt`** and **`clippy`**. From the repository root, `rustup show` should report that toolchain as active (rustup auto-installs it on first use).

**Pre-integration gate** — run before merging a factory branch to **`origin/main`**:

```bash
make gate
```

This executes [`scripts/gate.sh`](scripts/gate.sh) in order, stopping at the first failure:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace -- -D warnings`
3. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`
4. `cargo test --workspace`

The script exports **`CARGO_BUILD_JOBS=$(nproc)`** for the run so builds use all cores despite the **`jobs = 2`** cap in [`.cargo/config.toml`](.cargo/config.toml). Each stage prints wall time on success or failure.

**Integration procedure (maintainers and factory integrators):**

1. Rebase the branch onto current **`origin/main`**.
2. On that rebased branch, run **`make gate`** and confirm it exits **0**.
3. Run **`but merge`** only when the gate passes.

For day-to-day edits before committing, you can run individual stages or narrower tests (for example **`cargo test -p grit-lib --lib`**); the gate is the required bar for integration.

**`cargo clippy --workspace -- -D warnings`** must pass locally before you push; CI treats any Clippy warning as a failure (same invocation as **`make gate`**).

## Continuous integration

GitHub Actions workflow: [`.github/workflows/ci.yml`](.github/workflows/ci.yml). It runs on push to **`main`**, on **pull requests**, and via **workflow_dispatch**. Superseded PR runs are cancelled via workflow **`concurrency`**.

**Toolchain:** Rust **1.99.0** with **`rustfmt`** and **`clippy`**, pinned in [`rust-toolchain.toml`](rust-toolchain.toml) for local and agent checkouts, and in CI with [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain) (`uses: dtolnay/rust-toolchain@1.99.0`). **Bump procedure:** set `channel` in `rust-toolchain.toml`, update the `@1.99.0` tag in every job in `ci.yml` (keep all jobs on the same version), run `rustup toolchain install` for the new release locally, then refresh this section and [`AGENTS.md`](AGENTS.md) if the minimum stable version changes.

**Build parallelism:** Local checkouts cap Cargo jobs at 2 in [`.cargo/config.toml`](.cargo/config.toml) for multi-worktree development. CI exports **`CARGO_BUILD_JOBS=$(nproc)`** so runners use all cores; the config file is unchanged.

**Caching:** [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) with a distinct **`shared-key`** per job; caches are saved only on **`main`**.

| Job | What it runs | Reproduce locally |
| --- | --- | --- |
| **docs** | `make docs-check` and `python3 -m unittest discover scripts/tests` | `make docs-check && python3 -m unittest discover scripts/tests` |
| **fmt** | `cargo fmt --all --check` | `cargo fmt --all --check` |
| **clippy** | `cargo clippy --workspace -- -D warnings` | `CARGO_BUILD_JOBS=$(nproc) cargo clippy --workspace -- -D warnings` |
| **rustdoc** | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` | `make doc` |
| **test** | `cargo test -p grit-lib -p grit-cli`, then builds `grit` + `grit-http-server` and runs the transport tests | See [Running tests](#running-tests) and [Transport tests](#transport-tests-fetch-and-push-over-smart-http) |

Each job uses **`ubuntu-latest`** (the **docs** job uses **`timeout-minutes: 2`**; the others use **15**), and the jobs run in parallel.

## Adding tests

- Put **library behavior** in `grit-lib/src/**` unit tests or `grit-lib/tests/*.rs` integration tests.
- Use **`grit-test-support`** helpers for temp repos and fixtures shared across crates.
- For new **public API**, add at least one test that exercises the happy path and important errors.
- When behavior must match Git, assert it against the system `git` binary in the test (formats, protocols, `fsck`), not against copied Git output text.
- Do **not** weaken or delete tests to make a change pass; fix the implementation or adjust the test when requirements change.

## Benchmarks

Performance work compares `grit-lib` operations and `grit` commands against system `git` on the same machine (see **ROADMAP.md** item 2). Use Criterion benchmarks in `grit-lib` for library operations and `grit-bench` (`grit-utils`) for command-level comparisons, and record results when changing hot paths.

### Criterion (`grit-lib`)

Criterion micro-benchmarks live under `grit-lib/benches/` (harness disabled). Shared fixture modules build deterministic repos in a temp directory using `grit-lib`; the object suite calls the system `git` binary only for `index-pack` / `repack` when a real `.idx` is required.

Run the full suites locally:

```bash
cargo bench -p grit-lib --bench objects
cargo bench -p grit-lib --bench worktree
cargo bench -p grit-lib --bench history
```

CI runs a one-iteration smoke pass (every benchmark once):

```bash
cargo bench -p grit-lib --bench objects -- --test
cargo bench -p grit-lib --bench worktree -- --test
GRIT_HISTORY_BENCH_COMMITS=2000 cargo bench -p grit-lib --bench history -- --test
```

Before changing benchmarks, keep bench code warning-free:

```bash
cargo clippy -p grit-lib --benches -- -D warnings
```

**`objects`** groups: SHA-1 throughput, Git object-id hashing, zlib inflate/deflate on typical blob and tree payloads, loose and packed object reads (whole objects and deep delta chains), pack `.idx` lookup (hit/miss on small and large indexes), and delta apply.

**`worktree`** groups: index read and write at 10k and 100k entries (v2 and v4), config load with global/local layering (~500 keys plus `[include]` files), ignore matching (realistic `.gitignore` set against 100k paths), and `.gitattributes` lookup for 100k paths.

**`history`** (fixture helper `benches/s7/mod.rs`) exercises revwalk (topological and date order, with `RevListOptions::use_commit_graph` on/off against an on-disk commit-graph), rev-parse of common spec shapes, tree-to-tree diff on wide (10k-entry) and deeply nested trees with few and many changes, and blob diff (Myers and histogram) at small, 10k-line, and pathological sizes. The default fixture builds 10k commits; override with `GRIT_HISTORY_BENCH_COMMITS` (must exceed `100` for `HEAD~100` specs; CI smoke uses `2000`).

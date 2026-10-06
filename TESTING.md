# TESTING.md — Grit Test Strategy

## Overview

Grit validates behavior with **Rust tests** in the workspace crates. The primary product is **`grit-lib`**; **`grit-cli`** and **`grit-git`** add integration tests where CLI wiring matters.

1. **Unit and integration tests in `grit-lib`** — core Git semantics (objects, packs, refs, index, diff, revwalk, config, transport, and related areas). Prefer typed API tests over shelling out.
2. **Coverage tests for every public library interface** — each public API surface gets explicit tests; line-coverage targets are in **ROADMAP.md**.
3. **Workspace integration tests** — `grit-cli`, `grit-git`, `grit-examples`, and `grit-lib/tests/` for end-to-end flows (temp repos, file:// or local HTTP as needed).

The upstream Git shell harness and vendored Git C tree are **removed**. Compatibility with Git on-disk formats and wire protocols is enforced by Rust tests and **`bench/`** comparisons against the system `git` binary where useful.

## Running tests

```bash
# Library (required before every commit)
cargo test -p grit-lib --lib

# Broader workspace
cargo test --workspace

# Release build sanity (optional)
cargo test -p grit-lib --lib --release
```

### Lint and format

```bash
cargo fmt --check
cargo check --workspace
```

For **`cargo clippy --workspace --all-targets -- -D warnings`**, the workspace still carries pre-existing lint debt outside touched crates; fix warnings in code you change. Do not treat a full-workspace clippy run as a green gate until that debt is burned down.

## Adding tests

- Put **library behavior** in `grit-lib/src/**` unit tests or `grit-lib/tests/*.rs` integration tests.
- Use **`grit-test-support`** helpers for temp repos and fixtures shared across crates.
- For new **public API**, add at least one test that exercises the happy path and important errors.
- Do **not** weaken or delete tests to make a change pass; fix the implementation or adjust the test when requirements change.

## Benchmarks

Performance work uses **`bench/`** (see **ROADMAP.md** item 2). Compare grit against system `git` on the same machine; record JSON results when changing hot paths.

## `grit-git`

`grit-git` remains a Git-compatible CLI for users who need drop-in `git` behavior. It is tested with Rust integration tests in `grit-git/tests/` and via library tests for shared logic. It is no longer driven by a ported upstream test suite.

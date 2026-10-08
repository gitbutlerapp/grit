# TESTING.md — Grit Test Strategy

## Overview

Grit is **`grit-lib`** (the library) and **`grit-cli`** (the `grit` client). Behavior is validated with **Rust tests** in the workspace crates:

1. **Unit and integration tests in `grit-lib`** — core Git semantics (objects, packs, refs, index, diff, revwalk, config, transport, serving, and related areas). Prefer typed API tests over shelling out.
2. **Coverage tests for every public library interface** — each public API surface gets explicit tests; line-coverage targets are in **ROADMAP.md**.
3. **Workspace integration tests** — `grit-lib/tests/`, `grit-cli`, and `grit-examples` for end-to-end flows (temp repos, `file://`, local smart HTTP).
4. **Scope guards** — `grit-lib/tests/pruned_modules.rs` fails if removed grit-git-only module files or `mod` declarations reappear under `grit-lib/src` (see `docs/v1-scope.md`).
5. **Library hygiene ratchet** — `grit-lib/tests/hygiene.rs` (included in **`cargo test --workspace`** / **`make gate`**) walks `grit-lib/src`, strips `#[cfg(test)]` items, and counts patterns that must not grow in production library code: printing macros, `process::exit`, direct environment reads, Git-style `fatal:`/`error:`/`hint:`/`warning:` string literals, `Command::new`, process-global `static`/`thread_local!` state, and `SystemTime::now`. Counts are compared to [`grit-lib/hygiene-baseline.toml`](grit-lib/hygiene-baseline.toml); the test fails if any count increases, or decreases without updating the baseline (ratchet). Exempt a line with `// hygiene: <reason>` on that line or the line above. Regenerate the baseline after intentional reductions: `HYGIENE_REGEN=1 cargo test -p grit-lib --test hygiene hygiene_ratchet`.

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

**Toolchain:** Rust **1.99.0** with **`rustfmt`** and **`clippy`**, pinned in [`rust-toolchain.toml`](rust-toolchain.toml) for local and agent checkouts, and in CI with [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain) (`uses: dtolnay/rust-toolchain@1.99.0`). **Bump procedure:** set `channel` in `rust-toolchain.toml`, update the `@1.99.0` tag in every job in `ci.yml` and `release.yml` (keep all jobs on the same version; release builds install their cross targets for this toolchain), run `rustup toolchain install` for the new release locally, then refresh this section and [`AGENTS.md`](AGENTS.md) if the minimum stable version changes.

**Build parallelism:** Local checkouts cap Cargo jobs at 2 in [`.cargo/config.toml`](.cargo/config.toml) for multi-worktree development. CI exports **`CARGO_BUILD_JOBS=$(nproc)`** so runners use all cores; the config file is unchanged.

**Caching:** [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) with a distinct **`shared-key`** per job; caches are saved only on **`main`**.

| Job | What it runs | Reproduce locally |
| --- | --- | --- |
| **docs** | Site staleness, link check, and docs generator tests (see below) | `make docs-check && python3 -m unittest discover scripts/tests` |
| **fmt** | `cargo fmt --all --check` | `cargo fmt --all --check` |
| **clippy** | `cargo clippy --workspace -- -D warnings` | `CARGO_BUILD_JOBS=$(nproc) cargo clippy --workspace -- -D warnings` |
| **rustdoc** | Workspace API docs with warnings denied (see below) | `make doc` |
| **coverage** | `make coverage` (llvm-cov on `grit-lib`, floor ratchet) | `make coverage` |
| **test** | `cargo test -p grit-lib -p grit-cli`, then builds `grit` + `grit-http-server` and runs the transport tests | See [Running tests](#running-tests) and [Transport tests](#transport-tests-fetch-and-push-over-smart-http) |

Each job uses **`ubuntu-latest`** (the **docs** job uses **`timeout-minutes: 2`**; **coverage** uses **30**; the others use **15**), and the jobs run in parallel.

## Coverage

Line coverage for the object-database modules is measured with [`cargo llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and gated by [`scripts/coverage.py`](scripts/coverage.py) against [`grit-lib/coverage-floors.toml`](grit-lib/coverage-floors.toml).

**Run locally:**

```bash
make coverage
```

This builds `grit` into `target/llvm-cov-target/` (integration tests such as `precompose_system_git_roundtrip` need that binary via `GRIT_BIN`), runs `cargo llvm-cov -p grit-lib --lib --tests`, prints a per-module table (module, lines, missed, %), and fails if any tracked file, module group, or the **core** set (odb + pack* + midx + commit-graph) is below its floor. HTML and lcov reports land under `target/llvm-cov/html` and `target/llvm-cov/lcov.info`.

**Ratchet:** Floors are minimum allowed line-coverage percentages. After adding tests, raise floors with:

```bash
python3 scripts/coverage.py --input target/llvm-cov/summary.json --update
```

Each floor moves to `max(previous, current − 2.0)` rounded down to one decimal; the script never lowers an existing floor. Commit the updated `coverage-floors.toml` with the tests that improved coverage.

**CI:** The **coverage** job runs `make coverage` and uploads the HTML/lcov artifact. Unit tests for the gate live in `scripts/tests/test_coverage.py` and run under `python3 -m unittest discover scripts/tests` with the **docs** job.

### Upstream test mapping

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1006 | core object reads (`cat-file` -t/-s/-p, fsck); skip `--batch` UX | `grit-lib/tests/odb_loose_objects.rs` (`t1006_grit_written_loose_objects_git_cat_file_and_fsck`, `t1006_zlib_preset_dictionary_returns_needs_dictionary`) | covered |
| t1007 | `hash-object` / read loose objects | `grit-lib/tests/odb_loose_objects.rs` (`t1007_git_hash_object_w_grit_read_and_read_info`) | covered |
| t1050 | `hash-object` hashing and large blobs | `grit-lib/tests/odb_loose_objects.rs` (`t1050_odb_hash_matches_git_hash_object`, `large_blob_round_trip_grit_then_git_and_git_then_grit`) | covered |
| t1060 | loose object corruption and missing objects | `grit-lib/tests/odb_loose_objects.rs` (`t1060_*`) | covered |
| t1006/t1007 | commit/tree/tag parse and serialize vs git bytes | `grit-lib/tests/objects_parse_roundtrip.rs` | covered |
| t5300-pack-objects.sh | v2 pack index + reverse index byte-identical to `git index-pack --rev-index`; ingest passes `verify-pack` / `fsck --strict` | `grit-lib/tests/pack_ingest_roundtrip.rs` (`idx_and_rev_match_git_*`, `ingest_every_option_passes_git_verify_and_fsck_sha1`) | ported (core write/read) |
| t5303-pack-corruption-resilience.sh | apply_delta accept/reject vs git index-pack; pack corruption recovery | `grit-lib/tests/pack_deltas.rs` (`t5303_apply_delta_*`), `grit-lib/tests/pack_corruption.rs` | covered |
| t5309-pack-delta-cycles.sh | ref-delta cycles and cross-pack cycles (timeout guard) | `grit-lib/tests/pack_deltas.rs` (`ref_delta_*cycle*`) | covered |
| t5314-pack-cycle-detection.sh | self-referencing and two-object ref-delta cycles | `grit-lib/tests/pack_deltas.rs` (`ref_delta_self_reference*`, `ref_delta_two_object_cycle*`) | covered |
| t5316-pack-delta-depth.sh | deep OFS chains (50+) vs verify-pack depth | `grit-lib/tests/pack_deltas.rs` (`t5316_deep_ofs_chain*`) | covered |
| t5325-reverse-index.sh | RIDX `.rev` verify, corruption cases, `try_rev_positions_in_pack_order`, hashfile checksum | `grit-lib/tests/pack_ingest_roundtrip.rs` (`verify_pack_rev_*`, `try_rev_positions_*`, `hashfile_checksum_*`) | ported |
| t5351-unpack-large.sh | `unpack-objects` large blobs vs system git; strict missing reference | `grit-lib/tests/pack_ingest_roundtrip.rs` (`unpack_large_blob_*`, `unpack_strict_*`) | ported |

### Documentation site and rustdoc jobs

**`docs`** runs two steps (same as **`make docs-check`** plus generator unit tests):

1. **`make docs-check`** — builds `grit-lib` rustdoc (`RUSTDOCFLAGS="-D warnings" cargo doc -p grit-lib --no-deps`), renders docs and blog with **`scripts/site.py --check`** (fails if committed **`docs/docs/`** or **`docs/blog/`** differs from a fresh render), then **`scripts/linkcheck.py`** (internal `href`/`src` paths and `#` fragment anchors under **`docs/`**).
2. **`python3 -m unittest discover scripts/tests`** — manifest and sidebar rules, stable command URLs, **`rustdoc:`** link expansion against local **`target/doc`**, and benchmark table generation from committed **`grit-bench`** JSON (see [`AGENTS.md`](AGENTS.md) **Adding docs for a change**).

**`rustdoc`** — **`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`** (same as **`make doc`** and the rustdoc stage of **`make gate`**). This is the full workspace API surface; the **docs** job only needs **`grit-lib`** rustdoc to validate site links.

**CLI page contract** — the **`test`** job runs **`every_command_is_documented`** in **`grit-cli/src/main.rs`**: every subcommand has a page under **`content/docs/commands/`**, and each page documents every flag and nested subcommand (see the template in **`content/docs/commands/README.md`**).

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
cargo bench -p grit-lib --bench hot_paths -- --test
```

Before changing benchmarks, keep bench code warning-free:

```bash
cargo clippy -p grit-lib --benches -- -D warnings
```

**`objects`** groups: SHA-1 throughput, Git object-id hashing, zlib inflate/deflate on typical blob and tree payloads, loose and packed object reads (whole objects and deep delta chains), pack `.idx` lookup (hit/miss on small and large indexes), and delta apply.

**`worktree`** groups: index read and write at 10k and 100k entries (v2 and v4), config load with global/local layering (~500 keys plus `[include]` files), ignore matching (realistic `.gitignore` set against 100k paths), and `.gitattributes` lookup for 100k paths.

**`history`** (fixture helper `benches/s7/mod.rs`) exercises revwalk (topological and date order, with `RevListOptions::use_commit_graph` on/off against an on-disk commit-graph), rev-parse of common spec shapes, tree-to-tree diff on wide (10k-entry) and deeply nested trees with few and many changes, and blob diff (Myers and histogram) at small, 10k-line, and pathological sizes. The default fixture builds 10k commits; override with `GRIT_HISTORY_BENCH_COMMITS` (must exceed `100` for `HEAD~100` specs; CI smoke uses `2000`).

## Upstream Git scenario mapping (object-level)

Rust integration tests live under `grit-lib/tests/`. UX-only upstream cases are omitted. Reachability and alternates order are cross-checked against the system `git` binary where noted.

| Upstream | Rust test(s) | Notes |
| --- | --- | --- |
| t1060 (read from alternate) | `odb_alternates.rs` (`alternates_absolute_relative_comments_and_blank_lines`, chain/relative cases) | Relative `info/alternates`, read/exists via alternate |
| t5613 | `odb_alternates.rs` (full file) | Comments, blank lines, quoted paths, cycles, depth limit, corrupt local loose + good alternate |
| t5615 | `odb_alternates.rs` (`alternates_append_and_cache_refresh`, env alternates, write never in alternate) | Cache refresh / append / env `GIT_ALTERNATE_OBJECT_DIRECTORIES` |
| t5616 | `promisor_packs.rs` | Partial clone `--filter=blob:none`, promisor pack markers, `exists` vs `exists_local`, missing blob `ObjectNotFound` |
| t5330 | — | MIDX alternates scenarios covered in other steps; no additional rows for this change |

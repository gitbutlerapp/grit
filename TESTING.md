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

# Whole workspace, fast (what `make test` runs: nextest when installed, then doctests)
cargo nextest run --workspace && cargo test --workspace --doc
cargo test --workspace   # without nextest

# Everything, including git comparisons and heavy fixtures (`make test-full`, nightly CI)
make test-full
```

### Fast and full runs

The default run is unit and functional tests, and takes a few minutes. Tests that
start the system `git` (comparisons with Git, fixtures built with `git`) or build
heavy fixtures (scale, memory and benchmark-style tests, grit-bench scenarios)
are left out of it: each crate compiles them only with its `full-tests`
feature. `make test-full` enables that feature in every crate; CI runs it in the
nightly **test-full** job, not on every branch. That target builds release
`grit-cli` and sets **`GRIT_BIN`** for `grit-lib`'s `checkout_wide_tree_rss` test
(fork/`wait4` peak RSS against the CLI under test). CI and local nextest runs use
`scripts/nextest-checkout-wide-tree-rss-env.sh` (see `.config/nextest.toml`) to
build release `grit` and export **`GRIT_BIN`** before that test.

[`scripts/classify_tests.py`](scripts/classify_tests.py) decides which tests are
full-only: a test that calls `Command::new("git")`, or a helper that does
(`grit_test_support::git`, the `tests/support` harnesses, …), or that matches the
heavy patterns. Integration test files are gated as a whole in each crate's
`tests/integration.rs` (`#[cfg(feature = "full-tests")]`, written by
`scripts/sync_integration_tests.py`); unit tests in `src/` carry
`#[cfg_attr(not(feature = "full-tests"), ignore = …)]`
(`python3 scripts/classify_tests.py --mark-unit-tests` adds it).

```bash
# Run one full-only file
cargo test -p grit-lib --features full-tests --test integration merge_tree_git_compat::

# Release build sanity (optional)
cargo test -p grit-lib --lib --release
```

[cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`) runs the
workspace's ~900 test binaries in parallel, each test in its own process, so a test
that changes the working directory or environment cannot disturb another. Its
settings are in [`.config/nextest.toml`](.config/nextest.toml): tests slower than a
minute are reported, a test that hangs is stopped after four minutes, and the few
known timing-sensitive tests get a retry (a retried pass shows as flaky).

On macOS, the system checks every newly built executable the first time it runs
(`syspolicyd`), which can add seconds to each of those binaries and to the hook
scripts tests write. Exempting your terminal removes that cost: run
`sudo spctl developer-mode enable-terminal`, then enable the terminal under
System Settings → Privacy & Security → Developer Tools.

### Case-insensitive worktree tests

Run the dentry spelling regression on case-insensitive, case-preserving storage
(for example, default macOS storage or a Linux ciopfs mount). Point `TMPDIR` at
that volume so the test's temporary worktree uses its filesystem semantics:

```bash
TMPDIR=/path/to/case-insensitive-volume cargo test -p grit-lib --lib \
  path_icase::tests::dentry_spelling_skips_empty_case_duplicate_dir -- --exact --nocapture
```

Also run it on case-sensitive storage to cover the distinct-directory case.

### Release and benchmark binaries

The workspace root [`Cargo.toml`](Cargo.toml) defines two profiles:

| Profile | Used for |
| --- | --- |
| **`release`** | Fast workspace **`cargo build --release`** (including **`cargo test --release`**) — **`codegen-units = 16`**, no fat LTO |
| **`dist`** | Shipped **`grit`**, the GitHub **Release** workflow, and **`make build`** / **`grit-bench`** timings — **`lto = "fat"`**, **`codegen-units = 1`**, **`opt-level = 3`**, **`strip = true`**, **`panic = "unwind"`** (parallel pool in **`grit-lib`** uses **`catch_unwind`**) |

Build the CLI with **`cargo build --profile dist -p grit-cli`** (or **`make build`**). The binary is **`target/dist/grit`**. **`grit-bench`** resolves **`target/dist/grit`** before **`target/release/grit`**.

**Trivial status RSS check** (empty repo, factory VM methodology):

```bash
cargo build --profile dist -p grit-cli -p grit-utils
tmpdir=$(mktemp -d)
git -C "$tmpdir" init -q && git -C "$tmpdir" commit --allow-empty -m init
export GRIT_BENCH_MEASURE_CMD="$(pwd)/target/dist/grit status"
./target/release/grit-bench measure-rss --cwd "$tmpdir"
```

Peak RSS is printed in bytes (Linux **`getrusage(RUSAGE_CHILDREN)`** from the helper). Repeat in a loop or sample **`VmHWM`** from **`/proc/<pid>/status`** for acceptance comparisons. Step **2370** measured **~9.8 MiB** median on **`dist`**; the **8 MiB** goal remains and is tracked under factory **[#2348](https://github.com/gitbutlerapp/grit/issues/2348)**.

**`panic = "unwind"` on `dist`:** Audited **`grit-lib/src/parallel/pool.rs`** ( **`catch_unwind`** / **`resume_unwind`** on worker panics), **`grit-lib/src/hash/parallel.rs`** (tests), and test-only uses elsewhere. **`panic = "abort"`** would break parallel worker panic propagation; **`dist`** keeps **`unwind`**.

### CLI output snapshots

`grit-cli/tests/output_snapshots.rs` exercises human, `--json`, and `--markdown` output together via [`CliFixture`](grit-test-support/src/cli_fixture.rs) in **`grit-test-support`**. Each scenario runs the built `grit` binary three times and compares stdout, stderr, and exit status against committed [insta](https://insta.rs/) snapshots under `grit-cli/tests/snapshots/`.

```bash
cargo test -p grit-cli --test integration output_snapshots::
cargo test -p grit-cli --test integration output_snapshots_network::
```

Network snapshots use [`CliFixture::with_upstream`](grit-test-support/src/cli_fixture.rs) to publish the grit-built fixture to a bare upstream with system `git`, then exercise clone, fetch, pull, push, and `remote refs` from client repositories. Push scenarios run `git fsck --strict` on the upstream after the command; JSON stdout is checked for a single parseable value with no progress lines mixed in.

When you intentionally change CLI rendering or JSON/Markdown shape, refresh snapshots:

```bash
# Review and accept each diff interactively (requires `cargo install cargo-insta`)
cargo insta review

# Or update every snapshot in one shot (typical in CI/agent runs)
INSTA_UPDATE=always cargo test -p grit-cli --test integration output_snapshots::
INSTA_UPDATE=always cargo test -p grit-cli --test integration output_snapshots_network::
```

Commit the updated `.snap` files with the code change. CI sets `CI=true`, so a stale snapshot fails the test instead of silently updating.

### Transport tests (fetch and push over smart HTTP)

The smart-HTTP tests in `grit-lib/tests/` (`transport_http*`, `matrix_fetch`, `matrix_push`, `matrix_sha256`, `matrix_credentials`) start `grit-http-server`, which serves upload-pack and receive-pack **in-process** via `grit-lib` (no `grit` binary on `PATH` and no `GRIT_BIN`). The library client fetches and pushes against it, and `git` checks the results. They need the `http-ureq` feature and the `grit-http-server` binary built first; without it they print `SKIP:` and pass, so build before running:

```bash
cargo build -p grit-http-server
cargo test -p grit-lib --features http-ureq --test integration -- \
  transport_http matrix_fetch:: matrix_push:: matrix_sha256::
cargo test -p grit-lib --features http-ureq --test matrix_credentials
```

Add `-- --nocapture` and look for `SKIP:` lines to confirm nothing was skipped.

### Parser fuzzing (`grit-lib/fuzz`)

`grit-lib/fuzz/` is a **standalone** `cargo-fuzz` crate (not a workspace member; **nightly Rust only**). It exercises parser entry points in [`grit_lib::fuzz`](grit-lib/src/fuzz/mod.rs) for trees, commits, tags, the index, pkt-lines, protocol v2 responses, config, refspecs, ref names, transport URLs, dates, pack `.idx` files, and `packed-refs`.

**Stable replay (CI / `make gate`):** committed seeds and crash files under `grit-lib/fuzz/corpus/<target>/` are replayed by:

```bash
cargo test -p grit-lib --test integration fuzz_corpus_replay::
```

**Nightly fuzzing locally:**

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --locked
cd grit-lib/fuzz
# If `libfuzzer-sys` fails to compile C++ (missing `<cassert>`), use g++:
export CXX=g++ CC=gcc RUSTFLAGS="-C linker=g++"
cargo +nightly fuzz build
# One target, 10 minutes, using the seed corpus:
cargo +nightly fuzz run tree_parse -- -max_total_time=600 corpus/tree_parse
```

Regenerate seeds from system `git` after changing corpus layout:

```bash
grit-lib/fuzz/scripts/generate_corpus.sh
```

Scheduled **non-blocking** runs: [`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) (weekly, 10 minutes per target).

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
4. `cargo test -p grit-lib --test public_module_doctests`
5. `cargo nextest run --workspace` and `cargo test --workspace --doc` when cargo-nextest is installed, otherwise `cargo test --workspace`

The script exports **`CARGO_BUILD_JOBS`** as the CPU count (`nproc`, or `sysctl -n hw.ncpu` on macOS) for the run so builds use all cores despite the **`jobs = 2`** cap in [`.cargo/config.toml`](.cargo/config.toml). Each stage prints wall time on success or failure.

**Integration procedure (maintainers and factory integrators):**

1. Rebase the branch onto current **`origin/main`**.
2. On that rebased branch, run **`make gate`** and confirm it exits **0**.
3. Run **`but merge`** only when the gate passes.

For day-to-day edits before committing, you can run individual stages or narrower tests (for example **`cargo test -p grit-lib --lib`**); the gate is the required bar for integration.

**`cargo clippy --workspace -- -D warnings`** must pass locally before you push; CI treats any Clippy warning as a failure (same invocation as **`make gate`**).

## Public API snapshot

The committed file [`grit-lib/public-api.txt`](grit-lib/public-api.txt) is the semver-relevant public surface of **`grit-lib`**, generated with [`cargo-public-api`](https://github.com/cargo-public-api/cargo-public-api) on a pinned nightly toolchain. Pins live in [`scripts/public-api.sh`](scripts/public-api.sh) (`PUBLIC_API_NIGHTLY_DATE`, `CARGO_PUBLIC_API_VERSION`).

**Regenerate after intentional public API changes:**

```bash
make public-api
```

Review the diff, then commit `grit-lib/public-api.txt` with the code change.

**Check locally (same as CI):**

```bash
make public-api-check
```

The first run may install the pinned nightly toolchain and `cargo-public-api` (OpenSSL development headers are required to build the tool on Linux). The check is **not** part of **`make gate`** because installing nightly is slow; CI runs it in the dedicated **public-api** job.

## Continuous integration

GitHub Actions workflow: [`.github/workflows/ci.yml`](.github/workflows/ci.yml). It runs on push to **`main`**, on **pull requests**, nightly (**schedule**), and via **workflow_dispatch**. Superseded PR runs are cancelled via workflow **`concurrency`**.

**Toolchain:** Rust **1.99.0** with **`rustfmt`** and **`clippy`**, pinned in [`rust-toolchain.toml`](rust-toolchain.toml) for local and agent checkouts, and in CI with [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain) (`uses: dtolnay/rust-toolchain@1.99.0`). **Bump procedure:** set `channel` in `rust-toolchain.toml`, update the `@1.99.0` tag in every job in `ci.yml` and `release.yml` (keep all jobs on the same version; release builds install their cross targets for this toolchain), run `rustup toolchain install` for the new release locally, then refresh this section and [`AGENTS.md`](AGENTS.md) if the minimum stable version changes.

**Build parallelism:** Local checkouts cap Cargo jobs at 2 in [`.cargo/config.toml`](.cargo/config.toml) for multi-worktree development. CI exports **`CARGO_BUILD_JOBS=$(nproc)`** so runners use all cores; the config file is unchanged.

**Caching:** [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) with a distinct **`shared-key`** per job; caches are saved only on **`main`**, also when the job fails (`cache-on-failure`), so a red `main` still refreshes them.

Pushes and pull requests run only the fast jobs, the merge status. The nightly jobs run on the schedule, or on a branch with `gh workflow run ci.yml --ref <branch> -f full=true`; without `-f full=true` a dispatch runs the fast jobs only.

| Job | What it runs | Reproduce locally |
| --- | --- | --- |
| **scripts** | Python tests for repo scripts (coverage floors, upstream mapping) | `python3 -m unittest discover scripts/tests` |
| **fmt** | `cargo fmt --all --check` | `cargo fmt --all --check` |
| **clippy** | `cargo clippy --workspace -- -D warnings` | `CARGO_BUILD_JOBS=$(nproc) cargo clippy --workspace -- -D warnings` |
| **rustdoc** | Workspace API docs with warnings denied (see below) | `make doc` |
| **public-api** | `make public-api-check` (`cargo public-api` on pinned nightly; diff vs `grit-lib/public-api.txt`) | `make public-api-check` |
| **coverage** | Nightly and on demand only: `make coverage` (llvm-cov over every `grit-lib` test, `full-tests` included, floor ratchet) | `make coverage` |
| **test** | The fast run: `cargo nextest run --workspace --profile ci` (every test binary, in parallel, without stopping at the first failure) and `cargo test --workspace --doc` | `make test` |
| **test-full** | Nightly and on demand only: the same with `full-tests` on every crate (system `git` comparisons, heavy fixtures, `grit-bench` scenarios), the transport tests and Criterion smoke runs | `make test-full`; see [Transport tests](#transport-tests-fetch-and-push-over-smart-http) |
| **test-threaded** | Nightly and on demand only (the release build of every test binary takes ~26 min): `cargo test --release --workspace` with `GIT_CONFIG_COUNT` pairs forcing parallel config (`index.threads`, `pack.threads`, `checkout.workers`, `checkout.thresholdForParallelism=0`) | Same env vars as the workflow step in [`.github/workflows/ci.yml`](.github/workflows/ci.yml) |
| **miri** | Nightly and on demand only: `cargo +nightly miri test -p grit-lib --lib parallel::` over the scoped ordered pool unit tests | `rustup toolchain install nightly -c miri` then the command above |

### test-threaded (release workspace with parallel config)

Reproduce the CI job locally (install **Git ≥ 2.45** for reftable interop tests, same as the workflow):

```bash
export GRIT_REQUIRE_REFTABLE_GIT=1
export GIT_CONFIG_COUNT=4
export GIT_CONFIG_KEY_0=index.threads
export GIT_CONFIG_VALUE_0=4
export GIT_CONFIG_KEY_1=pack.threads
export GIT_CONFIG_VALUE_1=4
export GIT_CONFIG_KEY_2=checkout.workers
export GIT_CONFIG_VALUE_2=4
export GIT_CONFIG_KEY_3=checkout.thresholdForParallelism
export GIT_CONFIG_VALUE_3=0
cargo test --release --workspace
```

**Latest tracked run (2026-10-12, roadmap item 15 close-out):** [`test-threaded` on `main` run 38217276891](https://github.com/gitbutlerapp/grit/actions/runs/38217276891/job/114704849345) — still **failed** (bundle CLI + other workspace tests on tip `54ef4babb`; not parallelism). **`factory/parallel-bench-1006`** adds fixes for **`config_parse`** / Git **2.55**, ODB alternates, and reftable batch transactions; the local repro below passes those suites once that branch is checked out. **Replace this link with a green `test-threaded` job on `main` after those commits integrate.**

Each job uses **`ubuntu-latest`** (the **scripts** job uses **`timeout-minutes: 5`**; **public-api** and **test** use **30**; **test-threaded** **90**; **coverage** and **test-full** **120**; the others **15**), and the jobs run in parallel. The website has its own workflow, **[`site.yml`](.github/workflows/site.yml)** (see below).

## Coverage

Line coverage for the object-database modules is measured with [`cargo llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and gated by [`scripts/coverage.py`](scripts/coverage.py) against [`grit-lib/coverage-floors.toml`](grit-lib/coverage-floors.toml).

**Run locally:**

```bash
make coverage
```

This builds `grit` and `grit-http-server` into `target/llvm-cov-target/` (integration tests such as `precompose_system_git_roundtrip` need the `grit` binary via `GRIT_BIN`; smart-HTTP tests locate `grit-http-server` next to the test executable in that target dir), runs `cargo llvm-cov -p grit-lib --features http-ureq --lib --tests`, prints a per-module table (module, lines, missed, %), and fails if any tracked file, module group, named aggregate, the **total** line count across all of `grit-lib/src`, or the **core** set (odb + pack* + midx + commit-graph) is below its floor. HTML and lcov reports land under `target/llvm-cov/html` and `target/llvm-cov/lcov.info`.

**Module groups** in [`grit-lib/coverage-floors.toml`](grit-lib/coverage-floors.toml) include the ODB/pack/MIDX/commit-graph **core** groups, refs-area groups (`refs`, `reflog`, `config`, `ignore`, `attributes`), and history-area groups (`index`, `index-ext`, `diff`, `revwalk`, `revparse`, `merge`, `stash-notes-worktree`). The **refs-config** aggregate spans the refs-area groups; **history** spans the history-area groups. The `[total]` table sets a single floor over every `.rs` file under `grit-lib/src`. Legacy `[core]` and newer `[aggregate.<name>]` tables are both supported by [`scripts/coverage.py`](scripts/coverage.py).

**Ratchet:** Floors are minimum allowed line-coverage percentages. After adding tests, raise floors with:

```bash
python3 scripts/coverage.py --input target/llvm-cov/summary.json --update
```

Each floor moves to `max(previous, current − 2.0)` rounded down to one decimal; the script never lowers an existing floor. Commit the updated `coverage-floors.toml` with the tests that improved coverage.

**CI:** The **coverage** job runs `make coverage` nightly (and on `workflow_dispatch`) and uploads the HTML/lcov artifact. It needs the `full-tests` feature: the floors count the tests that start the system `git`. Unit tests for the gate live in `scripts/tests/test_coverage.py` and run under `python3 -m unittest discover scripts/tests` with the **scripts** job.

### Public function coverage

Every **public** function and method in `grit-lib` must be executed at least once by the combined **`--lib`**, integration **`--tests`**, and **`--doc`** runs that feed `make coverage`. The gate is [`scripts/public_fn_coverage.py`](scripts/public_fn_coverage.py):

- Public paths come from **rustdoc JSON** (`target/doc/grit_lib.json`), built with the same pinned nightly as [`scripts/public-api.sh`](scripts/public-api.sh) (see the **public-api** CI job).
- Execution counts come from **`cargo llvm-cov --json`** function records (demangled with **`rustfilt`**), merged across `target/llvm-cov/summary.json` and `target/llvm-cov/functions-doc.json`.
- Paths with zero executions must appear in [`grit-lib/public-fn-untested.txt`](grit-lib/public-fn-untested.txt). That allowlist **may only shrink**: the gate fails if an allowlisted path gains coverage (remove it in the same change as the new test) or if a public path is missing from both tests and the allowlist.

Add focused calls in `grit-lib/tests/<domain>_api_coverage.rs` (and existing sweeps where appropriate). For on-disk formats and wire protocol, cross-check with the system **`git`** binary the same way as other compatibility tests. Regenerate the allowlist after large test additions:

```bash
python3 scripts/public_fn_coverage.py --write-allowlist \
  --cov-json target/llvm-cov/summary.json \
  --cov-json target/llvm-cov/functions-doc.json
```

Unit tests for the gate live in `scripts/tests/test_public_fn_coverage.py`.

### Upstream test mapping: refs, reflog, config, ignore, attributes

Upstream `t/` scripts for refs, reflog, config, ignore rules, and attributes are mapped in the table below as those areas gain Rust coverage. Shared setup lives in [`grit-lib/tests/support/refs_harness.rs`](grit-lib/tests/support/refs_harness.rs): tests call `each_backend` to run the same scenario on **files** and **reftable** ref storage (`init_repository` with `ref_storage` `"files"` or `"reftable"`). Helpers run system `git` with a hermetic environment (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, fixed author/committer identity and dates). Grit ref lists come from `grit_lib::refs::list_refs`; git side uses `git show-ref`. When system git is older than 2.45 and cannot `git init --ref-format=reftable`, reftable scenarios print `SKIP: git lacks reftable` and return instead of failing. CI sets **`GRIT_REQUIRE_REFTABLE_GIT=1`** (with Git ≥ 2.45 from the git-core PPA in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)) so those skips become hard failures.

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1400-update-ref.sh | loose ref write/read round-trip; grit ↔ git `rev-parse` / `show-ref`; `git fsck --strict` after grit writes | `grit-lib/tests/refs_loose_symref.rs` (`t1400_loose_ref_roundtrip_both_backends`, `t1400_git_written_ref_read_by_grit_both_backends`) | covered (loose subset; packed/transaction UX skipped) |
| t1401-symbolic-ref.sh | symref read/write; HEAD detached vs symbolic; dangling symref; chain depth; long names; overwrite invalid symref | `grit-lib/tests/refs_loose_symref.rs` (`t1401_*_both_backends`) | covered |
| t0600-reffiles-backend.sh | empty directory must not block create/delete; non-empty directory and broken ref block create; short hash / trailing token parsing | `grit-lib/tests/refs_loose_symref.rs` (`t0600_*`; files backend only) | partial (lock/transaction cases in step 4) |
| t1405-main-ref-store.sh | `verify_refname_available_for_create` D/F prefix conflicts (single ref) | `grit-lib/tests/refs_loose_symref.rs` (`t1405_verify_refname_df_prefix_single_ref`) | partial (batch D/F in step 4) |
| t1430-bad-ref-name.sh | invalid storable names vs `git check-ref-format`; `write_ref` rejects | `grit-lib/tests/refs_loose_symref.rs` (`t1430_invalid_names_match_git_check_ref_format`, `t1430_write_ref_rejects_invalid_names_both_backends`) | covered |
| t1408-packed-refs.sh | load packed-refs (peeled/sorted headers, unsorted files, stale loose-over-packed, unicode names, garbage/CR/malformed line grammar vs valid refs) | `grit-lib/tests/refs_packed.rs` (`t1408_*`, `t1408_malformed_packed_refs_*`, `t1408_packed_refs_garbage_line_rejected_like_git`, `t1408_packed_refs_crlf_rejected_like_git`) | covered (files backend) |
| t0601-reffiles-pack-refs.sh | peeled annotated tags, `list_refs` / `list_refs_physical` / `list_refs_glob` vs `git for-each-ref`, namespace helpers | `grit-lib/tests/refs_packed.rs` (`t0601_peeled_and_sorted_traits_match_git`, `list_refs_physical_and_glob_merge_loose_over_packed`, `packed_refs_get_has_namespace_and_entry_exists`) | covered (files backend) |
| t0600-reffile.sh | delete loose+packed, packed-only, symref target; packed-refs lock failure; no stray lock files; no mkdir on packed-only delete | `grit-lib/tests/refs_packed.rs` (`t0600_delete_*`) | covered (files backend) |
| t1409-avoid-packing-refs.sh | post-clone remote-tracking batch into sorted packed-refs (leave `origin/HEAD` loose) | `grit-lib/tests/refs_packed.rs` (`pack_remote_tracking_refs_for_clone_matches_git`) | partial (clone pack layout only) |
| t0601 / fetch apply | `PackedRefs` snapshot + `resolve_ref_cached` / `write_ref_cached` after external `git pack-refs` | `grit-lib/tests/refs_packed.rs` (`resolve_ref_cached_and_write_ref_cached_reload_after_git_pack_refs`) | covered (files backend) |
| t1404-update-ref-errors.sh | CAS batch: bad `expected_old` applies nothing (refs + reflogs) | `grit-lib/tests/refs_transactions.rs` (`t1404_failed_cas_changes_nothing`) | covered |
| t1404-update-ref-errors.sh | D/F batch add/delete long vs short (loose + packed), both orders vs `git update-ref --stdin` | `refs_transactions.rs` (`t1404_df_conflicts_match_git_update_ref_stdin`) | covered (symref variants skipped) |
| t0031-lockfile-pid.sh | pre-existing `<ref>.lock` blocks write/delete; ref preserved | `refs_transactions.rs` (`ref_lock_present_fails_and_preserves_ref`) | covered (files backend) |
| t1400-update-ref.sh | `--stdin` CAS concurrency: one winner, `fsck` clean | `refs_transactions.rs` (`concurrent_cas_single_winner`) | covered |
| — | reftable failed transaction leaves `tables.list` unchanged | `reftable.rs` unit test (`write_transaction_inject_failure_leaves_tables_list_unchanged`) | covered |
| t1416-ref-transaction-hooks.sh | hook ordering | — | skipped (hooks covered elsewhere) |
| — | `update_refs` whole-batch lock-all atomicity vs Git `RefStore` transactions | — | gap → item 12 (RefStore transactions) |
| t3070-wildmatch.sh | wildmatch / iwildmatch / pathmatch / ipathmatch vectors; pathological `*` backtracking bound | `grit-lib/tests/wildmatch_vectors.rs` (`t3070_vectors_all_modes`, `t3070_pathological_wildmatch_under_time_bound`, ls-files pathmatch cross-check) | covered |
| t0008-ignores.sh | nested `.gitignore`, exclude, global excludes, negation, tracked overrides; `check-ignore -v -n` source/line/pattern | `grit-lib/tests/ignore_rules.rs` (`t0008_check_path_matches_git_check_ignore_verbose`, symlink warning, CLI precedence) | covered |
| t3001-ls-files-others-exclude | `--exclude-from`, `--exclude-per-directory` | `grit-lib/tests/ignore_rules.rs` (`t3001_exclude_from_and_per_directory_name`) | covered |
| t3003-ls-files-exclude | CRLF `.gitignore` lines | `grit-lib/tests/ignore_rules.rs` (`t3003_crlf_gitignore_lines`) | partial (core parse/match; skip UX-only ls-files flags) |
| t0600-reffiles-backend.sh | empty reflog file; `reflog expire --all`; symref expire vs referent | `grit-lib/tests/reflog_roundtrip.rs` (`reflog_exists_list_and_path_match_git`, `t0600_expire_on_symref_not_referent`) | covered (symref expire files backend only) |
| t1410-reflog.sh | reflog expire vs git (reachable/unreachable, gc patterns, stale-fix subset) | `grit-lib/tests/reflog_roundtrip.rs` (`t1410_expire_matches_git_byte_for_byte`, `expire_unreachable_stalefix_and_mark_reachable`) | covered (byte-compare on files backend) |
| t1411-reflog.sh | reflog line parsing (subset) | `grit-lib/tests/reflog_roundtrip.rs` (`git_written_reflog_lines_parse_like_grit`) | covered |
| t1413-reflog-detach.sh | detached HEAD + branch HEAD reflog mirror | `grit-lib/tests/reflog_roundtrip.rs` (`mirror_branch_reflog_to_head_and_detached`) | partial |
| t1417-reflog-updateref.sh | expire with `--updateref` | — | not applicable: no `--updateref` API |
| t1418-reflog-exists.sh | `reflog exists` / list | `grit-lib/tests/reflog_roundtrip.rs` (`reflog_exists_list_and_path_match_git`) | covered |
| t1421-reflog-write.sh | grit-written reflog bytes read by git | `grit-lib/tests/reflog_roundtrip.rs` (`t1421_grit_written_entries_read_by_git_log_g`) | covered |
| t0600 / rev-list | `all_reflog_oids` walk order | `grit-lib/tests/reflog_roundtrip.rs` (`all_reflog_oids_and_ordered_match_git_walk`) | covered |
| — | `core.logAllRefUpdates` modes vs git auto-create | `grit-lib/tests/reflog_roundtrip.rs` (`log_all_ref_updates_modes_match_git`) | covered |
| — | delete / truncate vs `git reflog delete` | `grit-lib/tests/reflog_roundtrip.rs` (`delete_reflog_and_truncate_match_git`) | covered (files backend byte-compare) |
| t1300-config.sh | config write: set/add/replace-all/unset/count, sections, quoting, multivar errors, lock file | `grit-lib/tests/config_write.rs` (`t1300_edit_matches_git_byte_for_byte`, `config_lock_present_is_typed_error_and_preserves_file`) | covered (read/parse subset on other steps; `--comment` cases grit-only on Git &lt; 2.46) |
| t1303-write-readonly.sh | write paths, subsection escaping, round-trip after edit | `grit-lib/tests/config_write.rs` (`t1300_edit_matches_git_byte_for_byte`, `grit_edited_repo_config_git_status_and_reread`) | partial (readonly-file cases N/A; subsection escape covered inside oracle `CASES`) |
| t1300 | config read (whitespace, escapes, continuations, bare keys, subsections); oracle vs `git config --file` | `grit-lib/tests/config_parse.rs` (`t1300_values_match_git_config_get`) | covered (read subset) |
| t1303 | wacky config (BOM, CRLF, long lines) | `grit-lib/tests/config_parse.rs` (`t1303_wacky_files_match_git`) | covered |
| t1305 | `[include]` / `[includeIf]` and `--show-origin` | `grit-lib/tests/config_includes.rs` (`t1305_include_and_includeif_match_git_show_origin`) | covered |
| t1308 | config set precedence (system/global/local/command) | `grit-lib/tests/config_parse.rs` (`t1308_config_set_precedence_matches_git`) | covered |
| t1309 | early config / protected / ceiling (read order) | `grit-lib/tests/config_includes.rs` (`read_early_config_matches_git_layers`, `load_protected_skips_repo_config`) | partial |
| t1310 | defaults, urlmatch, typed getters | `grit-lib/tests/config_parse.rs` (`t1310_config_default_and_urlmatch`, `typed_getters_match_git_config_type`) | covered |
| t1311 | optional includes / missing targets | `grit-lib/tests/config_includes.rs` (`optional_include_git_parity`, `missing_include_is_ignored`) | covered |
| t0003-attributes.sh | setup corpus: worktree, `--cached`, `--source` (tag-1/tag-2) vs `collect_attrs_for_path` | `grit-lib/tests/attributes_rules.rs` (`t0003_attrs_match_git_check_attr_worktree_cached_and_source`) | covered |
| t0003-attributes.sh | `[attr]` macros only at repo root / `info/attributes` | `attributes_rules.rs` (`t0003_macro_rules_only_top_level`) | covered |
| t0003-attributes.sh | `binary` macro, negative/`!` patterns, `**` globbing, `core.ignorecase` | `attributes_rules.rs` (`t0003_binary_macro_expansion_matches_git`, `t0003_negative_pattern_emits_warning`, `t0003_escaped_bang_pattern_matches_git`, `t0003_doublestar_patterns_match_git`, `t0003_ignorecase_worktree_matches_git`) | covered |
| t0003-attributes.sh | `info/attributes` precedence, overlong lines, quoted paths | `attributes_rules.rs` (`t0003_info_attributes_precedence_over_root`, `t0003_overlong_line_skipped_with_warning`, `t0003_quote_path_for_check_attr_matches_git`) | covered |
| t0003-attributes.sh | `builtin_objectmode` / invalid `builtin_*` names | `attributes_rules.rs` (`t0003_builtin_objectmode_*`, `t0003_validate_rules_for_add_rejects_bad_builtin_names`, `t0003_is_reserved_builtin_name`) | partial (skip when system git lacks `builtin_objectmode` in check-attr) |
| t0003-attributes.sh | command-line / stdin UX, symlink `.gitattributes`, 101 MiB files | — | skipped (UX-only or `EXPENSIVE`; optional `GRIT_RUN_EXPENSIVE_ATTR_TESTS`) |
| t0003-attributes.sh | bare repo default / `attr.tree` / bad `--attr-source` messages | — | follow-up (library loaders exist; extend `attributes_rules.rs`) |
| t0610-reftable-basics.sh | stack open/read, git↔grit ref and reflog read, grit writes + git fsck/refs verify, stack lock and update index | `grit-lib/tests/reftable_stack_interop.rs` (`git_written_reftable_read_by_grit`, `grit_written_reftable_passes_git_fsck_and_refs_verify`, `stack_tables_list_and_update_index_mechanics`, `interleaved_git_and_grit_writers_keep_update_index_monotonic`, `compaction_output_readable_by_git`, `concurrent_grit_writers_serialize_and_preserve_updates`) | covered (skip when git < 2.45 locally) |
| t0614-verify.sh | `git refs verify` on grit-written reftable stacks | `reftable_stack_interop.rs` (`grit_written_reftable_passes_git_fsck_and_refs_verify`, `concurrent_grit_writers_serialize_and_preserve_updates`) | covered when git provides `refs verify` |
| t1460-refs-migrate.sh | files → reftable migration command | — | skipped: no migration CLI/API in scope |
| t0612 (JGit interop) | JGit reftable quirks | — | skipped: JGit not in scope |

Detailed rows for remaining t1405 cases and other plan steps are filled in as later work lands; see also the ODB/pack mapping below.

### Upstream test mapping

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1006 | core object reads (`cat-file` -t/-s/-p, fsck); skip `--batch` UX | `grit-lib/tests/odb_loose_objects.rs` (`t1006_grit_written_loose_objects_git_cat_file_and_fsck`, `t1006_zlib_preset_dictionary_returns_needs_dictionary`) | covered |
| t1007 | `hash-object` / read loose objects | `grit-lib/tests/odb_loose_objects.rs` (`t1007_git_hash_object_w_grit_read_and_read_info`) | covered |
| t1050 | `hash-object` hashing and large blobs | `grit-lib/tests/odb_loose_objects.rs` (`t1050_odb_hash_matches_git_hash_object`, `large_blob_round_trip_grit_then_git_and_git_then_grit`) | covered |
| t1060 | loose object corruption and missing objects | `grit-lib/tests/odb_loose_objects.rs` (`t1060_*`) | covered |
| t1006/t1007 | commit/tree/tag parse and serialize vs git bytes | `grit-lib/tests/objects_parse_roundtrip.rs` | covered |
| t5300-pack-objects.sh | v2 pack index + reverse index byte-identical to `git index-pack --rev-index`; ingest passes `verify-pack` / `fsck --strict` | `grit-lib/tests/pack_ingest_roundtrip.rs` (`idx_and_rev_match_git_*`, `ingest_every_option_passes_git_verify_and_fsck_sha1`) | ported (core write/read) |
| t5302 | pack index v1/v2, forced 64-bit offset table, fanout edge cases, empty pack | `grit-lib/tests/pack_index_formats.rs` (`git_indexed_v1_v2_and_large_offset_table_oracle`, `fanout_*`, `empty_pack_index_from_git`) | covered |
| t5303-pack-corruption-resilience.sh | apply_delta accept/reject vs git index-pack; pack corruption recovery | `grit-lib/tests/pack_deltas.rs` (`t5303_apply_delta_*`), `grit-lib/tests/pack_corruption.rs` | covered |
| t5308 | duplicate pack entries remain readable | `grit-lib/tests/pack_index_formats.rs` (`duplicate_index_entries_remain_readable`) | covered |
| t5309-pack-delta-cycles.sh | ref-delta cycles and cross-pack cycles (timeout guard) | `grit-lib/tests/pack_deltas.rs` (`ref_delta_*cycle*`) | covered |
| t5313 | idx parse/read bounds: truncated idx, bad magic/version, trailer checksum, pack/index count skew, bogus offsets, ofs-delta | `grit-lib/tests/pack_index_formats.rs` (`truncated_idx_*`, `pack_index_object_count_mismatch_errors`, `bogus_offsets_and_ofs_delta_rejected_without_panic`) | covered |
| t5314-pack-cycle-detection.sh | self-referencing and two-object ref-delta cycles | `grit-lib/tests/pack_deltas.rs` (`ref_delta_self_reference*`, `ref_delta_two_object_cycle*`) | covered |
| t5316-pack-delta-depth.sh | deep OFS chains (50+) vs verify-pack depth | `grit-lib/tests/pack_deltas.rs` (`t5316_deep_ofs_chain*`) | covered |
| t5310-pack-bitmaps.sh | pack `.bitmap` read/write, `rev-list --test-bitmap`, lookup-table and hash-cache variants, corrupt EWAH/index fallback | `grit-lib/tests/pack_bitmap_read_git_compat.rs`, `grit-lib/tests/pack_bitmap_write_git_compat.rs`, `grit-lib/tests/rev_list_bitmaps.rs`, `grit-lib/tests/bitmap_walk.rs` | covered (skip JGit interop, incremental repack UX, `pack.preferBitmapTips` argv errors, boundary-traversal trace UX) |
| t5326-multi-pack-bitmaps.sh | MIDX bitmap write/read with embedded RIDX order, reuse, partial coverage, stale bitmap removal | `grit-lib/tests/pack_bitmap_read_git_compat.rs` (`reads_git_midx_bitmap`), `grit-lib/tests/midx_write_coverage.rs` (`compact_with_bitmaps_and_rev_sidecars`, `midx_reuse_tables_bitmaps_and_sidecars`) | partial (skip `--refs-snapshot`, fsck bitmap diagnostics UX, pseudo-merge) |
| t5327-multi-pack-bitmaps-rev.sh | MIDX bitmaps with separate `.rev` sidecar (not embedded RIDX) | `grit-lib/tests/midx_write_coverage.rs` (`compact_with_bitmaps_and_rev_sidecars`), `grit-lib/tests/pack_bitmap_read_git_compat.rs` (`reads_git_midx_bitmap`) | partial (same skips as t5326; grit always writes `.rev` for MIDX bitmaps) |
| t5318-commit-graph.sh | write/verify, Bloom/changed-paths, merge/octopus EDGE, corruption | `grit-lib/tests/commit_graph_roundtrip.rs`, `grit-lib/tests/commit_graph_corruption.rs` | ported (skip progress/option UX) |
| t5324-split-commit-graph.sh | split chain write/verify, layer load, alternates | `grit-lib/tests/commit_graph_roundtrip.rs`, `grit-lib/tests/commit_graph_corruption.rs` | ported (skip progress/option UX) |
| t5325-reverse-index.sh | RIDX `.rev` verify, corruption cases, `try_rev_positions_in_pack_order`, hashfile checksum | `grit-lib/tests/pack_ingest_roundtrip.rs` (`verify_pack_rev_*`, `try_rev_positions_*`, `hashfile_checksum_*`) | ported |
| t5328-commit-graph-64bit-time.sh | 64-bit commit times, generation overflow (GDO2) | `grit-lib/tests/commit_graph_roundtrip.rs` | ported (skip when platform git lacks 64-bit times) |
| t5351-unpack-large.sh | `unpack-objects` large blobs vs system git; strict missing reference | `grit-lib/tests/pack_ingest_roundtrip.rs` (`unpack_large_blob_*`, `unpack_strict_*`) | ported |
| t1450-fsck.sh | blob (always valid) | `grit-lib/tests/fsck_objects.rs` (`blob always ok`) | covered |
| t1450-fsck.sh | commit missing tree / author / committer | `fsck_objects.rs` | covered |
| t1450-fsck.sh | commit bad tree / parent oid | `fsck_objects.rs` | covered |
| t1450-fsck.sh | commit multiple authors | `fsck_objects.rs` | covered |
| t1450-fsck.sh | commit ident: missing email, bad name/email, spacing, date, timezone, overflow, zero-padded date | `fsck_objects.rs` | covered |
| t1450-fsck.sh | commit NUL in header / body | `fsck_objects.rs` | covered |
| t1450-fsck.sh | tree sort order, duplicate entries, modes, names (dot, dotdot, dotgit, HFS/NTFS, backslash), full path, null oid, truncation | `fsck_objects.rs` | covered |
| t1450-fsck.sh | tree large pathname | `fsck_objects.rs` (`tree large pathname`) | covered |
| t1450-fsck.sh | tag object/type/name/tagger headers and extra header after tagger | `fsck_objects.rs` | covered |
| t1450-fsck.sh | tag gpgsig / gpgsig-sha256 continuation | `fsck_objects.rs`, `fsck_standalone` unit tests | covered |
| t1450-fsck.sh | `.gitmodules` blob via dot-special fsck | `fsck_objects.rs` (`gitmodules_blob_fsck_*`) | covered |
| t1450-fsck.sh | `FsckError::report_line` formatting | `fsck_objects.rs`, `fsck_standalone::tests::fsck_error_report_line_format` | covered |
| t1450-fsck.sh | repository connectivity / missing objects | — | not applicable: no repository-level fsck API yet |
| t1450-fsck.sh | reflog entries | — | not applicable: no repository-level fsck API yet |
| t1450-fsck.sh | dangling / unreachable object reporting | — | not applicable: no repository-level fsck API yet |
| `t/t5319-multi-pack-index.sh` | Grit/git MIDX write and verify; preferred pack and RIDX; duplicate OID selection; large offsets (LOFF); stale MIDX after pack changes; `verify_midx` corruption diagnostics | `grit-lib/tests/midx_roundtrip.rs`, `grit-lib/tests/midx_corruption.rs`, `grit-lib/tests/midx_write_coverage.rs` | covered (skip Git MIDX v2 verify on older Git; bitmap UX cases skipped) |
| `t/t5334-incremental-multi-pack-index.sh` | Incremental chain layers; `resolve_midx_layer_path`; reads through `PreparedMidxChain`; convert to non-incremental via rewrite | `grit-lib/tests/midx_roundtrip.rs` (`incremental_chain_layer_paths_and_reads`; compact/chain tests skip when `git multi-pack-index --incremental` unavailable) | partial (bitmap/rev-list UX skipped) |
| `t/t5335-compact-multi-pack-index.sh` | `compact_multi_pack_index` success path and `CompactError` variants | `grit-lib/tests/midx_roundtrip.rs` (`compact_multi_pack_index_builds_verified_chain`, `compact_error_variants`) | partial (skips when incremental chain unavailable) |
| `t/unit-tests/u-reftable-record.c` | Ref/log record encode-decode, prefix compression | `grit-lib/tests/reftable_format.rs` (`ref_records_*`, `prefix_compression_*`, `log_*`) | covered |
| `t/unit-tests/u-reftable-block.c` | Block layout, restart points, padding | `grit-lib/tests/reftable_format.rs` (`block_size_*`, `restart_interval_*`, `unpadded_*`) | covered |
| `t/unit-tests/u-reftable-readwrite.c` | Writer/reader round-trip, index lookups | `grit-lib/tests/reftable_format.rs` (`index_block_lookups_find_every_ref`, `random_ref_and_log_sets_round_trip`) | covered |
| `t/unit-tests/u-reftable-table.c` | Table footer, CRC, corruption | `grit-lib/tests/reftable_format.rs` (`corruption_*`, `every_truncation_is_typed_error_no_panic`) | covered |
| `t/unit-tests/u-reftable-basics.c` | Empty table, update-index bounds, dump blocks | `grit-lib/tests/reftable_format.rs` (`empty_table_*`, `dump_reftable_blocks_stable_structure`) | covered |
| `t0613-reftable-write-options.sh` | Config-driven block size, restart interval, indexObjects | `grit-lib/tests/reftable_format.rs` (`write_options_block_size_and_restart_interval_applied`) | covered (geometricFactor parsed; stack compaction in later step) |
| `t0610-reftable-concurrent-writers.sh` | Many concurrent writers; ref transactions commit one table | `grit-lib/tests/refs_transactions.rs` (`concurrent_reftable_cas_single_winner`, `reftable_transaction_one_table_single_update_index`) | covered (loose-refs CAS paths in `refs_lock_coverage.rs`) |
| `t0612-reftable-jgit.sh` | JGit reftable interoperability | — | skipped (no JGit harness in CI; on-disk format covered by `reftable_format.rs`) |
| `t0614-refs-fsck.sh` | Refs verify / fsck diagnostics on loose and reftable backends | `grit-lib/tests/refs_fsck_coverage.rs`, `grit-lib/tests/reftable_format.rs` (`corruption_*`) | partial (repository-level `git fsck` parity deferred; ref-name and packed-refs checks covered) |
| `t1460-refs-migrate.sh` | `git refs migrate` loose ↔ reftable | — | gap (no `grit refs migrate` CLI; reftable read/write in library only — defer until migrate command lands) |

### Upstream test mapping: index, diff, revwalk, revparse, merge

History-area upstream `t/` scripts are mapped in the subsections below as Rust coverage lands. Shared setup for rev-list, rev-parse, and commit DAG fixtures lives in [`grit-lib/tests/support/history_harness.rs`](grit-lib/tests/support/history_harness.rs): hermetic system `git` (`GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` nulled, fixed identity, monotonic author/committer dates), [`commit_file`](grit-lib/tests/support/history_harness.rs), [`build_commit_dag`](grit-lib/tests/support/history_harness.rs), [`git_rev_list`](grit-lib/tests/support/history_harness.rs) / [`git_rev_parse`](grit-lib/tests/support/history_harness.rs), and [`skip_unless_git_at_least`](grit-lib/tests/support/history_harness.rs). Ref-backend helpers remain in [`refs_harness.rs`](grit-lib/tests/support/refs_harness.rs).

#### Index

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1600-index.sh | index versions 2/3/4 round-trip; grit ↔ git `ls-files -s`; `git fsck --strict` | `grit-lib/tests/index_versions_git_compat.rs` (`t1600_index_versions_roundtrip_with_git`) | covered |
| t1601-index-bogus.sh | corrupt index / REUC payload → [`Error::IndexError`] without panic | `index_versions_git_compat.rs` (`t1601_bogus_index_typed_errors`) | covered |
| t0090-cache-tree.sh | cache-tree / `write_tree` vs `git write-tree` | `grit-lib/tests/cache_tree_git_compat.rs` (`t0090_cache_tree_matches_git_write_tree`) | covered |
| t2030-unresolve-info.sh | REUC resolve-undo extension round-trip | `index_versions_git_compat.rs` (`t2030_resolve_undo_roundtrip`) | covered |
| t2104-update-index.sh | skip-worktree excluded from status like git | `grit-lib/tests/index_skip_worktree.rs` (`t2104_skip_worktree_excluded_from_status_like_git`) | covered |
| t2107-update-index-basic.sh | intent-to-add forces index v3 | `index_versions_git_compat.rs` (`t2107_intent_to_add_forces_index_version_three`) | partial (extended-flag subset) |
| t7011-ls-files-verbose.sh | assume-unchanged via `git ls-files -v` | `index_skip_worktree.rs` (`t7011_assume_unchanged_seen_by_git_ls_files_v`) | covered |
| t7012-ls-files-verbose.sh | skip-worktree bits vs `git ls-files -v` | `index_skip_worktree.rs` (`t7012_skip_worktree_bits_match_git_ls_files_v`) | covered |
| t1015-read-index-unmerged.sh | unmerged stages 1–3 round-trip | `index_versions_git_compat.rs` (`t1015_read_index_unmerged_stages_roundtrip`) | covered |

#### Index extensions

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1600-index.sh | EOIE/IEOT parallel load (existing step) | `grit-lib/tests/index_eoie_ieot_git_compat.rs` | covered (see ODB mapping) |
| t1700-split-index.sh | grit writes `link` + `sharedindex.<oid>`; system `git ls-files --stage` and `fsck --strict`; delete/replace EWAH bitmaps; `splitIndex.maxPercentChange` rebuild; `splitIndex.sharedIndexExpire` cleanup; `resolve_split_index_if_needed` on git split indexes | `grit-lib/tests/split_index_git_compat.rs` (`t1700_split_index_written_by_grit_read_by_git`, `t1700_git_split_index_read_by_grit`, `t1700_split_index_delete_and_replace_bitmaps_match_git`, `t1700_max_percent_change_rewrites_shared_index`, `t1700_shared_index_expire_removes_stale_file`, `split_index_link_extension_corrupt_rejected_without_panic`, `split_index_explicit_no_split_writes_unified_index`) | covered |
| t1701-racy-split-index.sh | racy entries re-verified under split index (content vs stat) | `grit-lib/tests/split_index_git_compat.rs` (`t1701_racy_split_index_entries_rechecked`) | covered |
| t7063-status-untracked-cache.sh | UNTR extension grit→git status reuse and git→grit; invalidation on add/remove; `dir_flags_from_config`; ident mismatch bypasses stale cache; `collect_untracked_from_cache` vs `git status --porcelain --untracked-files=all`; corrupt UNTR payloads | `grit-lib/tests/untracked_cache_git_compat.rs` (`t7063_untracked_cache_reused_by_git_status`, `t7063_git_untracked_cache_read_by_grit_status`, `t7063_invalidation_matches_git`, `t7063_dir_flags_from_config_modes`, `t7063_ident_mismatch_discards_stale_cache`, `untracked_extension_corrupt_payload_no_panic`, `index_name_hash_lazy_ignorecase_lookup`) | covered (skip git UX-only dump/progress cases) |
| t3008 lazy name-hash | case-insensitive `memihash` / lazy init thresholds | `grit-lib/tests/untracked_cache_git_compat.rs` (`index_name_hash_lazy_ignorecase_lookup`) | partial (test-tool dump parity deferred) |

#### Sparse

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1091-sparse-checkout-builtin.sh | non-cone list matches `git sparse-checkout list` | `grit-lib/tests/sparse_checkout_git_compat.rs` (`t1091_non_cone_sparse_list_matches_git`) | partial (core pattern/set; UX skipped) |
| t1092-sparse-checkout-compatibility.sh | cone patterns vs `git ls-files -t` / skip-worktree | `sparse_checkout_git_compat.rs` (`t1092_cone_patterns_match_git_skip_worktree`) | covered |
| t1011-read-tree-sparse-checkout.sh | `read-tree -mu` applies skip-worktree | `sparse_checkout_git_compat.rs` (`t1011_read_tree_sparse_checkout_applies_skip_worktree`) | covered |
| t1092-sparse-checkout-compatibility.sh | git-written sparse index (`sdir`) read or typed error | `sparse_checkout_git_compat.rs` (`git_written_sparse_index_sdir_load_or_typed_error`) | partial (full sparse-index write is a gap) |

#### Diff: blobs and algorithms

Shared harness: [`grit-lib/tests/support/diff_harness.rs`](grit-lib/tests/support/diff_harness.rs) (`git diff --no-index` oracle, `grit_blob_patch`, `assert_patch_matches_git`, ported [`BlobPair`](grit-lib/tests/support/diff_harness.rs) fixtures). Integration tests: [`grit-lib/tests/diff_blob_algorithms.rs`](grit-lib/tests/diff_blob_algorithms.rs).

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t4033-diff-patience.sh | patience / mixed-layout / unique blob pairs | `diff_blob_algorithms.rs` (`t4033_patience_matches_git`) | covered |
| t4050-diff-histogram.sh | histogram mixed-layout (apply) / unique (oracle) | `diff_blob_algorithms.rs` (`t4050_histogram_matches_git`) | partial (mixed-layout histogram hunk order) |
| t4071-diff-minimal.sh | minimal on shared alternative fixtures | `diff_blob_algorithms.rs` (`t4071_minimal_matches_git`) | partial (Myers-class fixtures; Git `--minimal` hunk shaping differs on stress cases) |
| t4065-diff-anchored.sh | `--anchored` single unique anchor | `diff_blob_algorithms.rs` (`t4065_anchored_matches_git`) | partial (multi-anchor cases deferred) |
| t4061-diff-indent.sh | `--indent-heuristic` / `diff.indentHeuristic` | `diff_blob_algorithms.rs` (`t4061_indent_heuristic_matches_git`) | covered (spaces.txt subset) |
| t4015-diff-whitespace.sh | trailing whitespace blob diffs; `-b`/`-w` oracle notes | `diff_blob_algorithms.rs` (`t4015_whitespace_modes_match_git`) | partial (default ws; `--ignore-space-at-eol` equivalence checked via git oracle) |
| t4012-diff-binary.sh | binary detection / parse git binary patch | `diff_blob_algorithms.rs` (`t4012_binary_patch_applies_with_git`) | partial (text patch parity; binary marker + apply parse) |
| t4052-stat-output.sh | `--numstat` counts and `--stat` change column | `diff_blob_algorithms.rs` (`t4052_numstat_matches_git`, `t4052_numstat_total_matches_git_stat_column`) | partial (counts only; `--stat` width layout is CLI presentation) |
| t4032-diff-inter-hunk-context.sh | `-U0` + `--inter-hunk-context` hunk count | `diff_blob_algorithms.rs` (`t4032_inter_hunk_context_matches_git`) | covered |
| t4029-diff-trailing-space.sh | trailing space line diff | `diff_blob_algorithms.rs` (`t4015_whitespace_modes_match_git`) | covered |
| t4074-diff-shifted-matched-group.sh | histogram shifted groups | `diff_blob_algorithms.rs` (`t4074_shifted_matched_group_histogram_applies_with_git`) | partial (patch applies; hunk body may differ from git histogram) |
| t4063-diff-blobs.sh | blob oid diff headers | — | gap (repository blob diff CLI; harness covers raw blob bytes) |
| t4051-diff-function-context.sh | `-W` function context | `diff_blob_algorithms.rs` (`t4051_function_context_applies_with_git`) | partial (apply parity; hunk extent may differ from git `-W`) |
| t4055-diff-context.sh | `diff.context` config | — | not ported |
| t4024-diff-optimize-common.sh | optimize common subexpression | — | not ported |
| t4049-diff-stat-count.sh | stat count limits | — | skipped (`--stat-count` is display UX) |
| t4047-diff-dirstat.sh | `--dirstat` | — | not ported |
| — | no newline at EOF markers | `diff_blob_algorithms.rs` (`no_newline_at_eof_pairs_match_git_or_apply`) | covered |
| — | random apply `--check` all algorithms | `diff_blob_algorithms.rs` (`random_patches_apply_with_git_all_algorithms`) | covered |

#### Diff: trees and renames

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t4000-diff-format.sh | raw `:mode mode oid oid status\tpath` lines vs `git diff-tree` / `diff-index` / `diff-files` | `grit-lib/tests/diff_tree_renames.rs` (`t4002_tree_index_worktree_raw_matches_git`, `t4006_diff_mode_raw_matches_git`) | partial (core raw format; not every t4000 fixture) |
| t4001-diff-rename.sh | `-M` rename detection scores and paths | `diff_tree_renames.rs` (`t4001_rename_detection_scores_match_git`) | covered |
| t4002-diff-basic.sh | tree↔tree, index↔tree, index↔worktree raw matrices | `diff_tree_renames.rs` (`t4002_tree_index_worktree_raw_matches_git`) | partial (representative matrix; not full canned tree corpus) |
| t4003-diff-rename-1.sh | rename edge cases | `diff_tree_renames.rs` (`t4001_rename_detection_scores_match_git`) | partial |
| t4005-diff-rename-2.sh | rename limits and scoring | `diff_tree_renames.rs` (`rename_limit_wide_tree_matches_git`) | partial |
| t4006-diff-mode.sh | mode-only and type bits in raw output | `diff_tree_renames.rs` (`t4006_diff_mode_raw_matches_git`) | partial |
| t4007-rename-3.sh | `-C` / `--find-copies-harder` vs `git diff-index` | `diff_tree_renames.rs` (`t4007_copy_detection_matches_git`) | covered |
| t4008-diff-break-rewrite.sh | `-B` / `-B -M` index↔tree raw (`M100`, renames) | `diff_tree_renames.rs` (`t4008_break_rewrite_matches_git`) | partial (primary t4008 scenarios) |
| t4009-diff-rename-4.sh | rename edge cases | `diff_tree_renames.rs` (`rename_limit_wide_tree_matches_git`) | partial |
| t4011-diff-symlink.sh | symlink type in tree diff | `diff_tree_renames.rs` (`t4023_typechange_matches_git`) | partial (typechange includes symlink rewrite) |
| t4022-diff-rename-sameness.sh | sameness and break | `diff_tree_renames.rs` (`t4008_break_rewrite_matches_git`) | partial |
| t4023-typechange.sh | blob↔symlink typechange raw `T` | `diff_tree_renames.rs` (`t4023_typechange_matches_git`) | covered (unix) |
| t4038-diff-combined.sh | merge `--cc` / `-c` combined raw | `grit-lib/tests/diff_combined.rs` (`t4038_combined_raw_matches_git`) | partial |
| t4043-diff-rename-binary.sh | binary rename detection | `diff_tree_renames.rs` (`t4001_rename_detection_scores_match_git`) | partial |
| t4046-diff-unmerged.sh | unmerged index stages | — | gap (follow-up with index step 3/4) |
| t4054-diff-bogus-tree.sh | corrupt tree → typed `CorruptObject` | `diff_tree_renames.rs` (`t4054_bogus_tree_typed_error`) | covered |
| t4057-diff-combined-paths.sh | combined path set vs `git diff -c --name-only` | `diff_combined.rs` (`t4057_combined_paths_octopus_matches_git`) | partial (two-parent merge scenario) |
| t4058-diff-duplicates.sh | duplicate path entries | — | gap |
| — | `diff.renameLimit` on wide trees (exact OID pairs) | `diff_tree_renames.rs` (`rename_limit_wide_tree_matches_git`), `diff_rename_parallel.rs` | covered |

#### Revwalk: ordering and limiting

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t6003-rev-list-topo-order.sh | `--topo-order`, `--date-order`, `--author-date-order` with/without `--reverse` on DAG with skewed author/committer dates | `grit-lib/tests/rev_list_ordering.rs` (`t6003_topo_date_author_date_order_match_git`, `skewed_and_equal_commit_dates_order_like_git`) | covered |
| t6013-rev-list-reverse-parents.sh | `--reverse --parents --full-history` on path-limited history; `--boundary` with `^root`; `--max-count=1 --boundary` with omitted merge | `rev_list_ordering.rs` (`t6013_reverse_matches_git`, `t6013_max_count_boundary_path_limited_parents_match_git`) | covered |
| t6005-rev-list-count.sh | `--max-count`, `--skip`, `--count` | `rev_list_ordering.rs` (`t6005_max_count_skip_match_git`) | covered |
| t6009-rev-list-parent.sh | `--first-parent`, `--min-parents`, `--max-parents`, `--no-merges` | `rev_list_ordering.rs` (`t6009_first_parent_min_max_parents_match_git`, `multiple_roots_and_octopus_merge_order_match_git`) | covered |
| t6019-rev-list-ancestry-path.sh | `--ancestry-path` on criss-cross DAG | `rev_list_ordering.rs` (`t6019_ancestry_path_matches_git`); `rev_list_upstream_t6019_t6000.rs` (`t6019_ancestry_path_upstream_cases_match_git`, `t6019_criss_cross_ancestry_path_empty_matches_git`) | partial (`--ancestry-path=<commit>` and `G..M -- G.t` matrix deferred) |
| t6100-rev-list-in-order.sh | `--in-commit-order --objects` interleaved OID sequence | `rev_list_ordering.rs` (`t6100_in_order_matches_git`) | covered |
| t6001-rev-list-graft.sh | `.git/info/grafts` and `git replace` navigation | `rev_list_ordering.rs` (`t6001_grafts_and_replace_refs_match_git`) | partial (core read side; replace via `git replace -f`) |
| t6000-rev-list-misc.sh | `A..B`, `^rev`, `--boundary`; `--unpacked --objects` | `rev_list_ordering.rs` (`t6000_misc_ranges_and_boundary_match_git`); `rev_list_upstream_t6019_t6000.rs` (`t6000_rev_list_unpacked_objects_matches_git`) | covered (core misc + unpacked object walk) |
| t6014-rev-list-all.sh | `--all` over multiple root refs | `rev_list_ordering.rs` (`t6014_rev_list_all_match_git`) | covered |
| t4217-log-limit.sh | `--since` / `--until` committer-date cutoffs; `--boundary` with date filters | `rev_list_ordering.rs` (`t4217_log_limit_since_until_match_git`, `t4217_boundary_since_until_match_git`) | covered |
| — | Symmetric `A...B` with `--left-right` / `--cherry-mark` / `--cherry-pick` (patch-id equivalence) | `rev_list_ordering.rs` (`symmetric_diff_left_right_cherry_mark_match_git`) | covered |
| — | `RevListOptions::use_commit_graph` on/off vs `git commit-graph write` for ordering walks | `rev_list_ordering.rs` (`ordering_identical_with_and_without_commit_graph`, `t6003_topo_date_author_date_order_match_git`, `t6019_ancestry_path_matches_git`, `t6100_in_order_matches_git`) | covered |
| — | Two-dot counts with Git- and grit-written commit-graphs, empty/reversed ranges, merges, multiple tips and clock skew | `bounded_limit_list.rs` (`deep_range_count_with_git_and_grit_commit_graphs_matches_git`, `graph_range_counts_across_merges_and_skewed_dates_match_git`); `merge_base::tests::graph_range_counts_are_bounded_by_distance` bounds parent lookups for both public count paths | covered |

#### Revwalk: simplification and objects

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t6000-rev-list-misc.sh (simplify) | `--simplify-by-decoration` with `--boundary` (decorated root/tip, middle pruned) | `rev_list_ordering.rs` (`simplify_by_decoration_boundary_match_git`) | covered |
| `t6002-rev-list-bisect.sh` | `--bisect-all` distances vs `select_bisect_commits` | `rev_list_simplify_objects.rs` (`t6002_bisect_all_matches_git`) | covered |
| `t6004-rev-list-path-optim.sh` | path-limited trivial parent optimization | `rev_list_simplify_objects.rs` (`t6004_path_limited_trivial_optimization_matches_git`) | covered |
| `t6012-rev-list-simplify.sh` | default / `--full-history` / `--simplify-merges` / `--first-parent` path limiting | `rev_list_simplify_objects.rs` (`t6012_simplification_modes_match_git`) | covered |
| `t6016-rev-list-graph-simplify-history.sh` | `--graph` history layout (not commit OID lists) | not applicable | skip |
| `t6017-rev-list-stdin.sh` | `--stdin` revision pseudo-options | `rev_list_simplify_objects.rs` (`t6017_stdin_revision_specs_match_git_semantics`); `rev_list_stdin_ref_glob_git_compat.rs` (`stdin_branches_tags_remotes_match_git`, `stdin_glob_and_tag_patterns_match_git`) | covered |
| `t6018-rev-list-glob.sh` | `--glob` ref patterns from stdin | `rev_list_simplify_objects.rs` (`t6018_ref_globs_and_exclude_match_git`); `rev_list_stdin_ref_glob_git_compat.rs` (`stdin_glob_and_tag_patterns_match_git`) | covered |
| `t6021-rev-list-exclude-hidden.sh` | `uploadpack.hideRefs` exclusion | `rev_list_simplify_objects.rs` (`t6021_exclude_hidden_refs_match_git`) | covered |
| `t6022-rev-list-missing.sh` | `--missing=` with `--objects` | `rev_list_simplify_objects.rs` (`t6022_missing_actions_match_git`) | covered |
| `t6102-rev-list-unexpected-objects.sh` | typed errors for type-mismatched object walks | `rev_list_simplify_objects.rs` (`t6102_unexpected_object_types_typed_error`) | covered |
| `t6110-rev-list-sparse.sh` | `--first-parent --boundary` on `HEAD^..` | `rev_list_simplify_objects.rs` (`t6110_first_parent_boundary_matches_git`) | covered |
| `t6111-rev-list-treesame.sh` | TREESAME / ancestry-path path walks | `rev_list_simplify_objects.rs` (`t6111_treesame_matches_git`) | covered |
| `t6112-rev-list-filters-object.sh` | `--filter=` object walks | `rev_list_simplify_objects.rs` (`t6112_object_filters_match_git`) | covered |
| `t6113-rev-list-bitmap-filters.sh` | `--use-bitmap-index` with `--filter=blob:none` | `rev_list_bitmaps.rs` (`t6113_blob_none_filter_with_bitmap_matches_git`) | covered (blob:none subset) |
| `t4216-log-bloom.sh` | Bloom vs no-Bloom commit-graph walks | `rev_list_simplify_objects.rs` (`t4216_bloom_and_no_bloom_identical`) | covered |

#### Rev-parse

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1500-rev-parse.sh | core verify specs (`HEAD`, refs, `^`, `~`, `@`, peelers) | `rev_parse_syntax.rs` (`t1500_core_specs_match_git_rev_parse`) | covered |
| t1503-rev-parse-verify.sh | missing ref rejected | `rev_parse_syntax.rs` (`t1503_verify_rejects_missing_ref`) | covered |
| t1505-rev-parse-last.sh | `@{-N}` branch stack | `rev_parse_syntax.rs` (`t1505_rev_parse_last_at_minus_n_matches_git`) | covered |
| t1506-rev-parse-diagnosis.sh | ambiguous path vs rev; `HEAD:path`, `:path`, index stages | `rev_parse_syntax.rs` (`t1506_ambiguous_path_vs_rev_typed_errors`, `colon_path_and_index_stage_match_git`, `rev_colon_path_treeish_blob_matches_git`) | covered |
| t1507-rev-parse-upstream.sh | `@{upstream}`, `@{u}`, `@{push}`, branch tracking | `rev_parse_syntax.rs` (`t1507_upstream_and_push_shorthand_match_git`) | covered |
| t1508-at-combinations.sh | `@{0}`, `ref@{0}` reflog tip | `rev_parse_syntax.rs` (`t1508_at_combinations_match_git`, `reflog_at_n_and_date_match_git`) | covered |
| t1511-rev-parse-caret.sh | `^{}`, `^{commit}`, `^{tree}`, `^{/msg}`, `^!`, `^@`, `^-N` | `rev_parse_syntax.rs` (`t1511_caret_peel_forms_match_git`) | covered |
| t1512-rev-parse-disambiguation.sh | short oid / `core.disambiguate` / `--short` | `rev_parse_syntax.rs` (`t1512_disambiguation_matches_git`) | covered |
| t1514-rev-parse-push.sh | `push.default` and `@{push}` | `rev_parse_syntax.rs` (`t1514_rev_parse_push_default_matches_git`) | covered |
| t6101-rev-parse-parents.sh | parent selectors `^N`, merge parents | `rev_parse_syntax.rs` (`t6101_rev_parse_parents_match_git`) | covered |
| — | `:/message`, range split `..` / `...` | `rev_parse_syntax.rs` (`colon_message_search_matches_git`, `split_range_helpers_match_git_endpoints`) | covered |

#### Merge

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t6010-merge-base.sh | merge-base, `--all`, `--octopus`, `--independent`, `--is-ancestor` | `merge_base_git_compat.rs` (`t6010_merge_base_variants_match_git`) | covered |
| t6010-merge-base.sh | `--fork-point` reflog ref name | `merge_base_git_compat.rs` (`fork_point_reflog_matches_git_t6010_fixture`) | covered |
| t6010-merge-base.sh | commit-graph vs walk | `merge_base_git_compat.rs` (`merge_base_identical_with_and_without_commit_graph`) | covered |
| t6099-merge-base-side-exhaustion.sh | `--all` drops ancestor among candidates; with/without commit-graph | `merge_base_git_compat.rs` (`t6099_side_exhaustion_merge_base_matches_git`) | covered |
| t6400-merge-df.sh | file/directory conflicts; `dir~<commit>` escape paths | `grit-lib/tests/merge_ort_git_compat.rs` (`t6400_df_conflicts_match_git_merge_tree`) | covered |
| t6402-merge-rename.sh | rename + content merge vs merge-tree | `merge_ort_git_compat.rs` (`t6402_rename_merges_match_git_merge_tree`) | partial (core rename subset) |
| t6403-merge-file.sh | line merge, diff3/zdiff3, union, marker size | `grit-lib/tests/merge_file_git_compat.rs` (`t6427_diff3_and_zdiff3_markers_match_git_merge_file`, `t6417_merge_union_and_marker_size_match_git_merge_file`) | partial (whitespace flags: grit API only) |
| t6405-merge-symlinks.sh | symlink three-way conflicts | `merge_ort_git_compat.rs` (`t6405_symlink_merge_matches_git`) | covered |
| t6406-merge-attr.sh | merge drivers (binary, union, text, custom) | `merge_ort_git_compat.rs` (`t6406_merge_drivers_match_git`), `merge_file_git_compat.rs` (union), `merge_driver_git_compat.rs` (custom driver, `merge=ours`, `%L`) | partial (full text driver matrix deferred) |
| t6407-merge-binary.sh | binary merge conflicts | `merge_ort_git_compat.rs` (`t6407_binary_conflict_matches_git`) | covered |
| t6408-merge-up-to-date.sh | fast-forward index composition | `grit-lib/src/porcelain/merge.rs` (`compose_fast_forward_index_keeps_staged_additions`) | partial |
| t6411-merge-filemode.sh | mode-only add/add conflicts | `merge_ort_git_compat.rs` (`t6411_filemode_merge_matches_git`) | covered |
| t6414-merge-rename-nocruft.sh | rename without crud | `merge_ort_git_compat.rs` (rename scenarios) | partial |
| t6415-merge-dir-to-symlink.sh | directory ↔ symlink | — | gap (follow-up) |
| t6416-recursive-corner-cases.sh | single merge-base corner cases | `merge_ort_git_compat.rs` (rename/delete subset) | partial |
| t6417-merge-ours-theirs.sh | `-Xours`/`-Xtheirs` favor | `merge_file_git_compat.rs` (`t6417_merge_favor_ours_theirs_grit_api`), `merge_ort_git_compat.rs` (`t6417_merge_ours_theirs_favor_matches_git`) | partial |
| t6418-merge-text-auto.sh | text=auto / binary detection | `merge_file.rs` (`is_binary_detects_nul_byte`) | partial |
| t6422-merge-rename-corner-cases.sh | rename/delete and rename/rename | `merge_ort_git_compat.rs` (`t6422_rename_corner_cases_match_git_merge_tree`) | partial |
| t6425-merge-rename-delete.sh | rename/delete | `merge_ort_git_compat.rs` (`t6422_rename_corner_cases_match_git_merge_tree`) | partial |
| t6427-diff3-conflict-markers.sh | diff3 / zdiff3 markers | `merge_file_git_compat.rs` (`t6427_diff3_and_zdiff3_markers_match_git_merge_file`) | covered |
| t6423-merge-rename-detection.sh | directory rename detection | — | gap (directory renames not implemented in merge-ort path) |
| t6401/t6404/t6431 | criss-cross / virtual merge bases | `merge_ort_git_compat.rs` (`t6401_criss_cross_merge_matches_git`, `t6404_recursive_merge_matches_git`, `t6431_criss_cross_rename_delete_matches_git`, `criss_cross_*_matches_git`); `merge_check_git_compat.rs` (`criss_cross_*`) | partial (rename/rename and directory/file conflicts between the merge bases are not settled the way Git's inner merges are) |
| t4301-merge-tree-write-tree.sh | `--write-tree` oracle | `merge_ort_git_compat.rs` (`t4301_merge_tree_write_tree_clean_merge_matches_git`) | covered |
| — | cherry-pick tree vs `git cherry-pick --no-commit` | `merge_ort_git_compat.rs` (`cherry_pick_tree_remap_matches_git`) | covered |

#### Stash/notes/worktrees/patch-ids

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t3903-stash.sh | grit stash create/list/show/pop; git reads grit stashes; git stash apply via grit | `grit-lib/tests/stash_git_compat.rs` (`t3903_grit_stash_read_by_git_and_reverse`, `grit_stash_create_matches_git_list_show_pop_and_fsck`) | covered |
| t3905-stash-include-untracked.sh | include-untracked stash shape vs `git stash show -p` | `stash_git_compat.rs` (`t3905_include_untracked_stash_shape_matches_git`, `grit_stash_with_untracked_matches_git`) | covered |
| t3908-stash-in-worktree.sh | stash push from linked worktree; main repo `git stash list` | `stash_git_compat.rs` (`t3908_stash_in_linked_worktree`) | covered |
| t3301-notes.sh | notes add/overwrite; grit ↔ `git notes show` / `git notes list` | `grit-lib/tests/notes_git_compat.rs` (`t3301_notes_roundtrip_with_git`) | covered |
| t3303-notes-subtrees.sh | read nested notes trees into flat entries | `notes_git_compat.rs` (`t3303_notes_subtrees_read_flat_entries`) | covered |
| t3305-notes-fanout.sh | read git fanout layouts; grit rewrite passes `git fsck --strict` | `notes_git_compat.rs` (`t3305_fanout_layouts_read_and_written`) | covered |
| t3306-notes-prune.sh | remove note entries / drop notes ref | `notes_git_compat.rs` (`t3306_notes_prune_via_remove`) | covered |
| t3308/t3309/t3310/t3311 notes-merge | merge strategies ours/theirs/union/cat_sort_uniq vs `git notes merge -s`; manual conflicts keep live notes ref | `notes_git_compat.rs` (`t3309_notes_merge_strategies_match_git`, `t3309_manual_notes_merge_keeps_local_ref_on_conflict`); `notes.rs` (`notes_merge_manual_leaves_conflict_commit`) | covered |
| t2400-worktree-add.sh | linked worktree admin layout; `git worktree list` / `list_worktrees` | `grit-lib/tests/worktree_git_compat.rs` (`t2400_worktree_admin_dir_roundtrip_with_git`, `worktree_linked_git_compat.rs`) | covered |
| t2402-worktree-list.sh | list linked worktrees vs porcelain | `worktree_git_compat.rs` (`t2402_worktree_list_matches_git_porcelain`) | covered |
| t2404-worktree-config.sh | `extensions.worktreeConfig` + `config.worktree` via `copy_filtered_worktree_config` | `worktree_git_compat.rs` (`t2404_worktree_config_matches_git`) | covered |
| t2401 worktree remove | linked worktree removal vs git | `grit-lib/tests/worktree_git_compat.rs` (`t2401_remove_linked_worktree_matches_git`) | covered (library `remove_linked_worktree`; no grit `worktree remove` CLI yet) |
| t2403/t2406/t2407 worktree | move/repair/prune/exclusive heads | — | gap (no grit `worktree move/repair/prune` yet; branch exclusivity covered in `worktree_extended_git_compat.rs`) |
| t4204-patch-id.sh | `PatchIdMode` stable/unstable/verbatim vs `git patch-id`; ODB `compute_patch_id`, including header-only entries (empty files, mode changes, pure renames) in multi-file commits | `grit-lib/src/patch_ids.rs` (`t4204_patch_id_modes_match_git`, `stable_patch_ids_match_git_patch_id_on_log_p`, `empty_file_add_and_delete_patch_ids_match_git`) | covered |
| — | per-worktree refs `main-worktree/` / `worktrees/<id>/` storage | `worktree_git_compat.rs` (`per_worktree_refs_resolve_like_git`); `worktree_ref.rs` unit tests | covered |
| — | stash apply/pop conflicts keep entry | `stash_git_compat.rs` (`pop_keeps_entry_on_conflict`); `stash_apply_git_compat.rs` | covered |

#### Supporting

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t1510-repo-setup.sh | `GIT_DIR` / `GIT_WORK_TREE` discovery vs git | `repo_setup_git_compat.rs` (`t1510_repo_setup_matches_git`, `t1510_repo_setup_git_work_tree_without_git_dir`) | covered |
| t1504-ceiling-dirs.sh | `GIT_CEILING_DIRECTORIES` stops discovery above ceiling | `repo_setup_git_compat.rs` (`t1504_ceiling_directories_blocks_walk_above_ceiling`) | partial |
| t6130-pathspec-wildcards.sh | glob / bracket pathspecs vs `git ls-files` | `pathspec_git_compat.rs` (`t6130_pathspec_glob_bracket_filename_matches_git`) | partial |
| t6132-pathspec-exclude.sh | `:/!` exclude magic vs git | `pathspec_git_compat.rs` (`t6132_pathspec_exclude_matches_git`) | covered |
| t4200-rerere.sh | rerere preimage records readable by git | `rerere_git_compat.rs` (`t4200_rerere_records_readable_by_git`) | partial |
| t4204-patch-id.sh | (see stash/notes section) | `patch_ids.rs` (`t4204_patch_id_modes_match_git`) | covered |
| — | CRLF autocrlf round-trip normalization | `supporting_modules_sweep.rs` (`crlf_roundtrip_lf_crlf_matches_git_normalization`) | covered |
| — | `.gitmodules` / submodule gitdir layout helpers | `supporting_modules_sweep.rs` (`gitmodules_and_submodule_gitdir_helpers`) | partial |
| — | merge/rebase/cherry-pick in-progress detection | `supporting_modules_sweep.rs` (`state_detect_and_repo_state_on_fresh_repo`); `state_git_am_in_progress.rs` | partial |
| t6040-tracking-branches.sh | ahead/behind vs `git rev-list --left-right --count` | `branch_tracking_git_compat.rs` (`t6040_branch_tracking_ahead_behind_matches_git`) | covered |
| — | `git blame --porcelain` vs `blame_file` | `blame_git_compat.rs` (`blame_matches_git_porcelain_after_adcb_permutation`) | covered |
| — | unified diff parse on git worktree diff | `apply_git_compat.rs` (`apply_parse_accepts_git_worktree_diff`) | partial |
| — | `git interpret-trailers --parse` | `interpret_trailers_git_compat.rs` (`interpret_trailers_parse_matches_git`) | covered |
| — | `git interpret-trailers --trailer` placement (`--where` / `--if-exists` / `--if-missing`), `trailer.*` config, output options | `interpret_trailers_git_compat.rs` (`trailer_placement_matrix_*`, `trailer_command_config_matches_git`, `trailer_output_options_match_git`) | covered |
| — | `git commit --signoff` / `git cherry-pick -x [-s]` message rewriting | `commit_trailers_git_compat.rs` | partial (single-line trailer-only subject differs under `cherry-pick -x`) |
| — | `git log --date=<mode>` vs `show_date` | `git_date_git_compat.rs` (`git_log_date_iso_matches_show_date`) | partial |

### Documentation site and rustdoc jobs

The website (grit-scm.com) is the Next.js app in **`site/`**, built from **`content/`** at deploy time; no generated HTML is committed. The **[`Site`](.github/workflows/site.yml)** workflow runs on pull requests and on pushes to **`main`** that touch the site's inputs (`content/`, `site/`, `grit-examples/`, `grit-lib/`, `grit-utils/baselines/`), with **`GRIT_SITE_STRICT=1`**:

1. **`cargo doc -p grit-lib --no-deps`** — local rustdoc, used to resolve **`rustdoc:`** links to docs.rs and to generate the API map.
2. **`npm run check`** — loads every page: manifest rules (every Markdown file listed in **`content/docs/site.toml`**), code fences with a language, includes, benchmark baselines, **`rustdoc:`** paths that must exist, and every internal link and `#` anchor in docs and blog.
3. **`npm test`** (vitest: content model, layouts, highlighting, agent Markdown outputs) and **`npm run typecheck`**.
4. **`npm run build`** on pull requests; on **`main`**, **`vercel build`** + **`vercel deploy --prebuilt --prod`** to the **`grit-scm`** project (Vercel team *Side Projects*).

Reproduce locally with **`make docs-check`**; preview with **`make docs`** (dev server at http://localhost:3000).

**`rustdoc`** — **`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`** (same as **`make doc`** and the rustdoc stage of **`make gate`**). This is the full workspace API surface; the **Site** workflow only needs **`grit-lib`** rustdoc to resolve site links.

**CLI page contract** — the **`test`** job runs **`every_command_is_documented`** in **`grit-cli/src/main.rs`**: every subcommand has a page under **`content/docs/commands/`**, and each page documents every flag and nested subcommand (see the template in **`content/docs/commands/README.md`**).

## Adding tests

- Put **library behavior** in `grit-lib/src/**` unit tests or `grit-lib/tests/*.rs` integration tests.
- Use **`grit-test-support`** helpers for temp repos and fixtures shared across crates. Run system `git` with `grit_test_support::git` (hermetic identity, date and config), resolve revisions with `grit_test_support::rev_parse`, and reserve server ports with `grit_test_support::free_port` (its handout list is process-wide, so modules in one test binary never collide) instead of adding per-file copies.
- Integration tests in `grit-lib`, `grit-cli`, `grit-utils`, `grit-examples`, `grit-protocol` and `grit-http-server` build as **one test binary per crate**, `tests/integration.rs`, which includes each `tests/*.rs` file as a module. After adding, renaming or removing a test file, run **`python3 scripts/sync_integration_tests.py`**; the `every_test_file_is_compiled` test fails until you do. Run one file's tests with `cargo test -p <crate> --test integration <file>::`. A file that changes the working directory or environment, or runs its own binary again by test name, gets a `[[test]]` entry in the crate's `Cargo.toml` instead, so it keeps its own process. Shared helper directories (`tests/support/`) are declared once in `integration.rs`; a test file uses them with `use crate::support;`.
- For new **public API**, add at least one test that exercises the happy path and important errors.
- When behavior must match Git, assert it against the system `git` binary in the test (formats, protocols, `fsck`), not against copied Git output text.
- Do **not** weaken or delete tests to make a change pass; fix the implementation or adjust the test when requirements change.

## Benchmarks

Performance work compares `grit-lib` operations and `grit` commands against system `git` on the same machine (see **ROADMAP.md** item 2). Use Criterion benchmarks in `grit-lib` for library operations and `grit-bench` (`grit-utils`) for command-level comparisons, and record results when changing hot paths.

### One command per suite (`make bench-lib`, `make bench-scenarios`)

From the repository root (requires **hyperfine** on `PATH` for scenario runs):

```bash
make bench-lib                              # all Criterion benches → target/bench/criterion-summary.json
make bench-scenarios TIER=M                 # full scenario matrix (default tier M)
make bench-scenarios TIER=S SMOKE=1         # CI smoke: one iteration per scenario, small fixtures
```

| Target | Output | Schema |
| --- | --- | --- |
| `bench-lib` | `target/bench/criterion-summary.json` | `schema_version: 1`, `driver: "criterion"`, grit-only timings (ms) |
| `bench-scenarios` | `target/bench/scenarios-<host>.json` and `.md` | grit-bench v1 (git vs grit, `ratio` per scenario) |

**Tiers:** `TIER=S|M|L` selects the registry synthetic fixture (`synth-s` / `synth-m` / `synth-l`). **`SMOKE=1`** passes `--smoke` to `grit-bench scenarios` (exactly one hyperfine run, tiny network/parallel fixtures, skips ODB backend / bitmaps / refs).

CI job **`bench-scenarios-smoke`** (nightly and on demand only, as is **`bench-daily-smoke`**) installs hyperfine and runs `make bench-scenarios TIER=S SMOKE=1`. Failures mean a scenario argv or prepare hook broke; wall times are not compared to baselines.

Detail: [Benchmark harness](content/docs/benchmarking.md).

### Send-pack frontier regression

The `pack_frontier` Criterion target builds 1,000 advertised commit tips and one
backdated want. This makes repeated scans of the pending ancestry heap visible
without transport or compression dominating the measurement:

```bash
cargo bench -p grit-lib --bench pack_frontier
cargo test -p grit-lib --lib pack_object_select::tests
```

Tracked-staging unit tests count hash-only stream passes and stream writes, including
size changes, stale index stat sizes, missing old objects, and partial clones. The
`add_stream_size_git_compat` integration test cross-checks SHA-1 and SHA-256 index
and blob writes with system Git at both loose-object and streamed-pack thresholds.

### Criterion (`grit-lib`)

Criterion micro-benchmarks live under `grit-lib/benches/` (harness disabled). Shared fixture modules build deterministic repos in a temp directory using `grit-lib`; the object suite calls the system `git` binary only for `index-pack` / `repack` when a real `.idx` is required.

Run the full suites locally:

```bash
cargo bench -p grit-lib --bench objects
cargo bench -p grit-lib --bench worktree
cargo bench -p grit-lib --bench history
```

The **`midx`** suite measures warm lookups and cold preparation of both strict and
permissive views in a fresh repository cache. To use a large Git-written MIDX
instead of the small default fixture, set `GRIT_MIDX_BENCH_OBJECTS` to its object
directory (OS caches remain warm):

```bash
GRIT_MIDX_BENCH_OBJECTS=/path/to/repo/.git/objects \
  cargo bench -p grit-lib --bench midx -- midx_prepare
cargo test -p grit-lib --test mapped_cg_midx_git --test odb_conformance_midx --test midx_corruption
cargo test -p grit-cli --test status_midx_memory -- --nocapture
```

The Linux status regression creates a Git-written MIDX over 420,000 historical
blobs while HEAD contains one file. It compares subprocess peak RSS with and
without MIDX for human, short, JSON, and Markdown output, with system Git controls.

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

### Sparse reset and switch scaling

Issue #2081 uses Git-written sparse indexes over 120k and 240k tracked paths,
with 300 visible files and 400 excluded directory rows. The library regression
counts expanded sparse descendants per repository and requires zero for unchanged
excluded trees. A separate test removes an excluded tree object to prove that
building the target index never reads that subtree. Compatibility tests cover
nested cones, changed/deleted excluded trees, staged and unstaged files, gitlinks,
reset-to-HEAD repair, and Git operations after grit writes the index. The
`sparse_reset_present_child_parent_becomes_file` and
`sparse_reset_present_child_parent_becomes_symlink` regressions materialize an
excluded tracked child before resetting to a parent replacement. They check the
new HEAD, index rows and skip-worktree bits, full worktree contents, strict fsck,
and subsequent Git resets. An external sentinel must survive the symlink case.
Late sparse cleanup only considers paths still tracked in the target and never
follows symlinked parents; checkout handles deleted children before replacement.

```bash
cargo test -p grit-lib --lib sparse_reset_and_switch_do_not_expand
cargo test -p grit-lib --lib sparse_tree_walk_keeps_matching_excluded_subtree_opaque
cargo test -p grit-lib --test sparse_checkout_checkout
cargo test -p grit-cli --test reset_sparse_checkout_cli
cargo bench -p grit-lib --bench hot_paths -- checkout/sparse_300_visible
cargo run --release -p grit-utils -- --grit target/release/grit daily \
  --only reset/sparse@sparse-120k,reset/sparse@sparse-240k,switch/sparse@sparse-120k,switch/sparse@sparse-240k
```

Daily rows also include `reset/sparse@sparse-unique-120k` and
`switch/sparse@sparse-unique-120k`, with unique excluded blobs and 5,000 changed
excluded paths. Each timed iteration goes to the base commit and back; fixture
construction and post-run status/fsck checks stay outside the measurement.
Use `--smoke` for a 2,400-path fixture. Record both wall time and peak RSS, with
builds and tests stopped, against the same system Git and a frozen main binary.

### Linux GNU startup and loader contract

The x86_64 GNU v0.5.21 artifact runs on glibc 2.34 (Rocky Linux 9). Its
`GLIBC_2.39` version requirement is **weak**, so treating the highest version
string as its minimum would overstate the loader baseline. RELR introduces the
strong `GLIBC_ABI_DT_RELR` requirement and requires glibc 2.36; it cannot be
silently enabled for a binary that works on 2.34. The existing
`grit-x86_64-unknown-linux-gnu.tar.gz` stays on its original build path as the
default/fallback. Only `grit-x86_64-unknown-linux-gnu-relr.tar.gz` adds a CLI-only
`cargo rustc` final link with `-C link-arg=-Wl,-z,pack-relative-relocs` in
`.github/workflows/release.yml`; workspace profiles and dependency builds are
unchanged. Both assets contain the same commands and completions.

Stable and nightly installers always use the existing GNU asset on glibc
systems, including known glibc 2.36+ versions. The optional RELR asset is a manual
download only: final-tip review measurements found a 1–2% startup regression,
so loader compatibility alone is insufficient to select it automatically.
aarch64, musl, Apple and Windows are unchanged. Exercise the actual shell
installers with isolated platform and download fixtures; the tests must keep
stock selection even when RELR is supported or its download is unavailable:

```bash
cargo test -p grit-utils --test install_relr
python3 -m unittest scripts/tests/test_install.py
```

Check the actual artifact, including PIE, interpreter, allowed dependencies,
GNU_RELRO/BIND_NOW, non-executable stack, symbol versions, RELR ABI marking,
startup and randomized runtime load addresses:

```bash
python3 scripts/check-linux-release.py /path/to/stock/grit \
  --max-glibc 2.34 --relr forbidden --run
python3 scripts/check-linux-release.py /path/to/relr/grit \
  --max-glibc 2.36 --relr required --run
cargo test -p grit-utils --test release_elf
GRIT_RELEASE_ELF=/path/to/relr/grit cargo test -p grit-utils \
  --test release_elf candidate_release_elf_contract -- --ignored
```

The Rust regression tests link small C probes and reject missing RELR, non-PIE,
lazy binding and a newer strong loader requirement. Run the ignored artifact
contract against stock first (it must fail), then the candidate (it must pass).
Static checks do not establish runtime portability: run the binary's `--help`,
`branch`, `status` and `resolve HEAD` on the oldest supported loader as well.
Use the system's normal ASLR settings; the runtime guard keeps `upload-pack`
waiting for input and inspects four actual process mappings.

See [startup comparison methodology](content/docs/benchmarking.md#startup-relocation-comparison)
for both profiles, 60 warm measured runs per tool, realistic fixtures and RSS
cross-checks. RELR links are CLI-only; no workspace profile is changed. Apple,
Windows, static musl and GNU aarch64 need their own portability validation.

The release workflow checks the baseline artifact on pinned Rocky Linux 9
(glibc 2.34) and the RELR artifact on pinned Debian bookworm (glibc 2.36).
Each runs help, branch, status and resolve in a one-commit repository and
asserts the loader version. Both assets also run the ELF/runtime guard above
on the build host: PIE, interpreter/dependencies, GNU_RELRO/BIND_NOW, RELR ABI
present only in the optional artifact, startup smoke, and four distinct runtime
load addresses. ASLR settings are never modified. The baseline is packaged and
uploaded before the optional CLI relink, preserving its original build path.

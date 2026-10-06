# TESTING.md — Grit Test Strategy

## Overview

Grit validates behavior with **Rust tests** in the workspace crates. The primary product is **`grit-lib`**; **`grit-cli`** and **`grit-git`** add integration tests where CLI wiring matters.

1. **Unit and integration tests in `grit-lib`** — core Git semantics (objects, packs, refs, index, diff, revwalk, config, transport, and related areas). Prefer typed API tests over shelling out.
2. **Coverage tests for every public library interface** — each public API surface gets explicit tests; line-coverage targets are in **ROADMAP.md**.
3. **Workspace integration tests** — `grit-cli`, `grit-git`, `grit-examples`, and `grit-lib/tests/` for end-to-end flows (temp repos, file:// or local HTTP as needed).
4. **Upstream shell harness (regression gate)** — a curated subset runs in CI via **`scripts/run-tests.sh`** against **`grit-git`**. The full ported suite under **`tests/`** remains available for deeper compatibility work; pass counts for in-scope files **must not regress** when you touch harness-covered behavior (**do not weaken** harness tests to green a change).

Compatibility with Git on-disk formats and wire protocols is enforced primarily by Rust tests and **`bench/`** comparisons against the system `git` binary. The harness adds end-to-end **`grit-git`** coverage using the ported upstream scripts and vendored **`git/`** reference tree.

## Running tests (Rust)

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

### Manually skipped files

Edit that test's TOML (**`data/tests/<group>/<stem>.toml`**): set **`in_scope = "skip"`**. Skipped files are **never** executed (single-file, group, or full run). Their tests are **excluded** from the summary counts on **`docs/progress/index.html`**. They still appear on **`docs/testfiles.html`** with a skipped badge so you can see what was opted out.

Re-run **`python3 scripts/generate-test-files-catalog.py`** if you add or rename `.sh` files and want the status tree updated without running tests (otherwise the next `run-tests.sh` also refreshes the catalog; TOMLs for deleted test files are pruned).

## Scripts reference

| Script                                          | Role                                                                                                             |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `scripts/gate.sh`                               | Pre-integration gate: fmt, clippy (**`-D warnings`**), workspace tests; prints per-stage wall times.             |
| `scripts/test_status.py`                        | Shared helper: load/save **`data/tests/<group>/<stem>.toml`** files (atomic writes, TOML serialization, pruning). |
| `scripts/generate-test-files-catalog.py`        | Scan `tests/t*.sh`, merge the **`data/tests/`** tree (preserves `in_scope` and prior run results; prunes stale TOMLs). |
| `scripts/run-tests.sh`                          | Select files to run (`--list`), execute harness, strict CI mode (`--strict`), invoke apply (+ dashboard with `--dashboard`). |
| `scripts/apply-test-run-results.py`             | Merge one batch of run lines into the matching **`data/tests/`** TOMLs.                                          |
| `scripts/generate-dashboard-from-test-files.py` | Read `data/tests/` only; write **`docs/progress/index.html`**, **`docs/testfiles.html`**, and **`docs/test-progress.svg`**. |

## Data pipeline (step by step)

1. **`scripts/generate-test-files-catalog.py`** — Scans `tests/t*.sh`, counts `test_expect_success` / `test_expect_failure` per file, assigns `group` (`t0`–`t9` from the first digit of the `tNNNN…` prefix, matching **`git/t/README`** test families), and writes or merges the **`data/tests/<group>/<stem>.toml`** files. Invoked automatically at the start of **`run-tests.sh`**.

2. **`scripts/run-tests.sh`** — Copies `target/release/grit-git` to `tests/grit`, builds the file list (honoring **`in_scope`**), runs each selected script under `timeout`, parses the `# Tests:` summary line, writes a small batch TSV for **`scripts/apply-test-run-results.py`**.

3. **`scripts/apply-test-run-results.py`** — Updates the matching **`data/tests/`** TOMLs (`passed_last`, `failing`, `fully_passing`, `status`, etc.). Writes are atomic (temp file + rename), so parallel family runs never collide: each test file owns its own TOML.

4. **`data/tests/<group>/<stem>.toml`** keys (`file` and `group` are derived from the path, not stored):

| Key              | Meaning                                                                    |
| ---------------- | -------------------------------------------------------------------------- |
| `in_scope`       | `"yes"` or `"skip"` (manual)                                               |
| `tests_total`    | Count of test markers in the file                                          |
| `passed_last`    | Pass count from the last run                                               |
| `failing`        | Fail count from the last run                                               |
| `fully_passing`  | `true` if `tests_total > 0` and `failing == 0`                             |
| `status`         | `"ok"`, `"timeout"`, or `"error"` from the harness                         |
| `expect_failure` | Count of `test_expect_failure` lines                                       |

Example (`data/tests/t0/t0000-basic.toml`):

```toml
in_scope = "yes"
tests_total = 92
passed_last = 91
failing = 1
fully_passing = false
status = "ok"
expect_failure = 8
```

## Work strategy: one file at a time

1. Pick a test file that is not fully passing.
2. Run it: `./scripts/run-tests.sh t1234-foo.sh`
3. Fix Rust in `grit/` / `grit-lib/`.
4. Re-run until green; the test's status TOML updates automatically.

### Priority order

1. Plumbing (`t0xxx`, `t1xxx`)
2. Index/checkout (`t2xxx`)
3. Core commands (`t3xxx`)
4. Diff (`t4xxx`)
5. Transport (`t5xxx`)
6. Rev machinery (`t6xxx`)
7. Porcelain (`t7xxx`)
8. External helpers (`t9xxx`) last

## test_expect_failure

When you fix known breakage, flip `test_expect_failure` → `test_expect_success` in the test file.

## test-lib.sh

**Do not** modify `tests/test-lib.sh` casually — past changes caused regressions.

## Harness pitfall: cwd persists across tests (the `cd repo` trap)

Before "fixing grit" for a failing file, rule this out first — it is a **test-file bug, not a grit bug**.

**Symptom:** only the `setup` test passes (≈1/N) and every later test fails with
`./test-lib.sh: line NNNN: cd: repo: No such file or directory`.

**Cause:** `test-lib.sh` *persists* the working directory across top-level `test_expect_success`
blocks (matching upstream `git/t`). If the setup test does `git init repo && cd repo && …` it
leaves the shell **inside** `repo/`. Every later block that starts with a bare `cd repo` then runs
*before* it is back at the trash root, so the `cd` fails and the block aborts before any `git`/`grit`
command runs.

**Fix:** wrap each test body in a subshell so the `cd` cannot leak:
```sh
test_expect_success 'desc' '
	(
	cd repo &&
	…
	)
'
```
`scripts/_wrap_cd_subshell.py <files…>` does this mechanically (idempotent; only wraps bodies that
contain a `cd`). After wrapping, **re-run only the files you changed** and diff pass counts against
the previous recorded values — wrapping a body that a *currently-passing* test relied on for leaked cwd
can cost a test, so confirm no file regressed before committing.

**Spotting candidates:** low pass ratio **and** nearly every `test_expect_success` body starts with a
bare `cd`. Quick scan harness files with `rg -l "test_expect_success" tests/t*.sh` and inspect bodies that start with `cd`.

## Lint, format, and pre-integration gate

The repo root [`rust-toolchain.toml`](rust-toolchain.toml) pins **Rust 1.99.0** with **`rustfmt`** and **`clippy`**. From the repository root, `rustup show` should report that toolchain as active (rustup auto-installs it on first use).

**Pre-integration gate** — run before merging a factory branch to **`origin/main`**:

```bash
make gate
```

This executes [`scripts/gate.sh`](scripts/gate.sh) in order, stopping at the first failure:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`

The script exports **`CARGO_BUILD_JOBS=$(nproc)`** for the run so builds use all cores despite the **`jobs = 2`** cap in [`.cargo/config.toml`](.cargo/config.toml). Each stage prints wall time on success or failure.

**Integration procedure (maintainers and factory integrators):**

1. Rebase the branch onto current **`origin/main`**.
2. On that rebased branch, run **`make gate`** and confirm it exits **0**.
3. Run **`but merge`** only when the gate passes.

For day-to-day edits before committing, you can run individual stages or narrower tests (for example **`cargo test -p grit-lib --lib`**); the gate is the required bar for integration.

```bash
rustup show
cargo fmt --all --check
cargo check --workspace
```

## Continuous integration

GitHub Actions workflow: [`.github/workflows/ci.yml`](.github/workflows/ci.yml). It runs on push to **`main`**, on **pull requests**, and via **workflow_dispatch**. Superseded PR runs are cancelled via workflow **`concurrency`**.

**Toolchain:** Rust **1.99.0** with **`rustfmt`** and **`clippy`**, pinned in [`rust-toolchain.toml`](rust-toolchain.toml) for local and agent checkouts, and in CI with [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain) (`uses: dtolnay/rust-toolchain@1.99.0`). **Bump procedure:** set `channel` in `rust-toolchain.toml`, update the `@1.99.0` tag in every job in `ci.yml` (keep all jobs on the same version), run `rustup toolchain install` for the new release locally, then refresh this section and [`AGENTS.md`](AGENTS.md) if the minimum stable version changes.

**Build parallelism:** Local checkouts cap Cargo jobs at 2 in [`.cargo/config.toml`](.cargo/config.toml) for multi-worktree development. CI exports **`CARGO_BUILD_JOBS=$(nproc)`** so runners use all cores; the config file is unchanged.

**Caching:** [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) with a distinct **`shared-key`** per job; caches are saved only on **`main`**.

| Job | What it runs | Reproduce locally |
| --- | --- | --- |
| **fmt** | `cargo fmt --all --check` | `cargo fmt --all --check` |
| **test** | `cargo test -p grit-lib -p grit-cli` | `CARGO_BUILD_JOBS=$(nproc) cargo test -p grit-lib -p grit-cli` |
| **smoke** | Release **`grit-git`**, strict harness smoke list, then `git diff --exit-code` | See [Upstream shell harness](#upstream-shell-harness) (smoke commands); use an isolated `--data-dir` |

Each job uses **`ubuntu-latest`**, **`timeout-minutes: 15`**, and runs in parallel. The smoke job uploads per-file harness logs from **`$RUNNER_TEMP/smoke/logs/`** as a workflow artifact when it fails.

## Upstream shell harness

Build **`grit-git`** first; the runner copies **`target/release/grit-git`** to **`tests/grit`** and exposes it as `git` for the harness.

```bash
cargo build --release -p grit-git

# Single file
./scripts/run-tests.sh t3200-branch.sh

# CI smoke subset (strict, list-driven; isolated status + logs)
./scripts/run-tests.sh --strict --list data/ci/smoke-tests.txt --data-dir /tmp/smoke --no-catalog

# Isolated run: write results under another directory, leave data/tests/ untouched
./scripts/run-tests.sh --data-dir /tmp/iso t0000-basic.sh
```

Per-file status lives in **`data/tests/<group>/<stem>.toml`**. Use **`--data-dir`** when you must not rewrite tracked status files (CI smoke, local experiments). Dashboards under **`docs/`** regenerate only with **`--dashboard`** or **`scripts/generate-dashboard-from-test-files.py`**.

### CI smoke subset

**Purpose:** A fast, stable regression gate for CI and local pre-push checks. It runs a curated set of upstream harness files spanning core areas (object database, refs, index, config, diff, revision walking, transport) without executing the full suite.

**List file:** [`data/ci/smoke-tests.txt`](data/ci/smoke-tests.txt)

**How to run locally:**

```bash
cargo build --release -p grit-git
./scripts/run-tests.sh --strict --list data/ci/smoke-tests.txt --data-dir /tmp/smoke --no-catalog
```

- **`--strict`** — exit non-zero if **`--list`** was used with an empty or comment-only file, if any explicit **`--list`** (or argv) target does not resolve to a runnable harness file, if the resolved explicit list is empty, if any selected file has failing tests, a timeout/error status, or zero tests executed; print each failing file stem and its TAP `not ok` lines (works with or without **`--data-dir`**). With **`--data-dir`**, full harness output for each file is also written under `<data-dir>/logs/<stem>.log` for CI artifact upload. Regression: `python3 scripts/test_run_tests_strict_list.py`.
- **`--list`** — read harness file names from the path (one per line; `#` starts a comment).
- **`--data-dir`** — write status TOMLs and logs under an isolated directory so tracked `data/tests/` and dashboards stay unchanged.
- **`--no-catalog`** — skip refreshing the full status catalog (the smoke list names files explicitly).

**Measured runtime:** On a 4-core Linux VM with `target/release/grit-git`, five consecutive strict smoke runs (51 files) completed in **88–95 seconds** each (median ~92s), under the 3-minute target.

**Rules for adding or removing files:**

- Only include harness files that pass reliably on `main` (no flakes across repeated runs).
- Do **not** add files with **`in_scope = "skip"`** in their status TOML.
- Prefer broad coverage over duplicating the same subsystem many times.
- Remove or defer files that become nondeterministic or too slow for CI; document the reason here.
- **Excluded at baseline (planned for step 9 / racy-git):** `t3903-stash.sh`, `t4015-diff-whitespace.sh`, `t7600-merge.sh` — nondeterministic until fixed.
- **Dropped from smoke (intermittent):** `t4022-diff-rewrite.sh` — failed sporadically in back-to-back full-list runs (`detect rewrite`, `-B` deletion tests) while isolated reruns passed; re-add only after the flake is fixed and five consecutive full-list passes succeed.

**Files in the smoke list (51), by area:**

| Area | Files |
| ---- | ----- |
| basics | `t0000-basic.sh` |
| odb / packs | `t1006-cat-file.sh`, `t1007-hash-object.sh`, `t10060-hash-object-determinism.sh`, `t1450-fsck-basic.sh`, `t5300-pack-object.sh`, `t5300-unpack-objects.sh`, `t5302-pack-index.sh`, `t5304-prune.sh` |
| refs / reflog | `t1403-show-ref.sh`, `t1404-update-ref-errors.sh`, `t1410-reflog.sh`, `t10010-show-ref-dereference.sh`, `t3200-branch.sh`, `t6300-for-each-ref.sh` |
| index / worktree | `t0090-cache-tree.sh`, `t1000-read-tree-m-3way.sh`, `t1001-read-tree-m-2way.sh`, `t1700-split-index.sh`, `t2200-add-update.sh`, `t2400-worktree-add.sh`, `t3000-ls-files-others.sh`, `t3600-rm.sh`, `t3700-add.sh`, `t7102-reset.sh`, `t7508-status.sh` |
| config / ignore / attr | `t0001-init.sh`, `t0003-attributes.sh`, `t0008-ignores.sh`, `t1300-config.sh` |
| diff | `t4000-diff-format.sh`, `t4001-diff-rename.sh`, `t4002-diff-basic.sh`, `t4010-diff-pathspec.sh`, `t4013-diff-various.sh`, `t4017-diff-retval.sh` |
| revs | `t0062-revision-walking.sh`, `t1500-rev-parse.sh`, `t1503-rev-parse-verify.sh`, `t1506-rev-parse-diagnosis.sh`, `t1507-rev-parse-upstream.sh`, `t6000-rev-list-misc.sh`, `t6003-rev-list-topo-order.sh`, `t6006-rev-list-format.sh`, `t6009-rev-list-parent.sh`, `t6010-merge-base.sh`, `t6101-rev-parse-parents.sh`, `t6120-describe.sh` |
| transport | `t5510-fetch.sh`, `t5512-ls-remote.sh`, `t5516-fetch-push.sh` |

After dropping `t4022-diff-rewrite.sh`, five consecutive strict smoke runs on this branch passed all **51** listed files every time (see measured runtime above; re-measured after review fixes).

### Harness scripts

| Script | Role |
| ------ | ---- |
| `scripts/run-tests.sh` | Select files (`--list`), run harness, strict CI mode (`--strict`), apply results |
| `scripts/apply-test-run-results.py` | Merge run lines into **`data/tests/`** TOMLs |
| `scripts/generate-test-files-catalog.py` | Scan `tests/t*.sh`, maintain status tree |
| `scripts/generate-dashboard-from-test-files.py` | Regenerate harness dashboards from TOMLs |

## Adding tests

- Put **library behavior** in `grit-lib/src/**` unit tests or `grit-lib/tests/*.rs` integration tests.
- Use **`grit-test-support`** helpers for temp repos and fixtures shared across crates.
- For new **public API**, add at least one test that exercises the happy path and important errors.
- Do **not** weaken or delete tests to make a change pass; fix the implementation or adjust the test when requirements change.

## Benchmarks

Performance work uses **`bench/`** (see **ROADMAP.md** item 2). Compare grit against system `git` on the same machine; record JSON results when changing hot paths.

## `grit-git`

`grit-git` remains a Git-compatible CLI for users who need drop-in `git` behavior. It is tested with Rust integration tests in `grit-git/tests/`, library tests for shared logic, and the upstream shell harness (CI smoke subset and optional full runs).

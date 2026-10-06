# TESTING.md — Grit Test Strategy

## Overview

Testing follows **AGENTS.md — Project direction**:

1. **Convert relevant Git unit tests to Rust** — core functionality and edge cases from `git/t` / harness knowledge, exercised against the **`grit-lib`** public API. Skip command UX and option-compatibility matrices.
2. **Coverage tests for every public library interface** — each public API surface gets explicit tests; line coverage targets are in **ROADMAP.md** (testing items).
3. **Upstream harness as regression gate** — `tests/*.sh` run via **`grit-git`** (`scripts/run-tests.sh`). In-scope pass counts **must not regress**; **do not weaken** harness tests to green a change.

The harness is not the primary way we grow new coverage anymore; it guards **`grit-git`** compatibility. Mapping tables from upstream tests to Rust tests will live in this file as conversion work lands (ROADMAP items 9, 11, 16).

Harness execution: `target/release/grit-git` is copied to `tests/grit` and exposed as `git` by the runner.

The **single source of truth** for per-file harness status is the per-test TOML tree **`data/tests/<group>/<stem>.toml`** (e.g. `data/tests/t0/t0000-basic.toml`). There are no intermediate TSVs and no aggregate CSV. Dashboards — **`docs/index.html`** (homepage progress card), **`docs/progress/index.html`** (summary + progress by group), **`docs/testfiles.html`** (per-file table, filterable by group), and **`docs/test-progress.svg`** (overall pass-rate badge for the README) — are generated from that tree, but **only when requested**: pass `--dashboard` to `run-tests.sh` or run `python3 scripts/generate-dashboard-from-test-files.py` directly.

## Running tests

Build the binary first; the runner expects **`target/release/grit-git`**.

```bash
cargo build --release -p grit-git

# Single file
./scripts/run-tests.sh t3200-branch.sh

# Prefix run: `tests/<arg>*.sh` (e.g. `t1` matches all `t1*.sh` harness files)
./scripts/run-tests.sh t1

# Full suite (every status TOML with in_scope = "yes")
./scripts/run-tests.sh

# Options (can appear before or after the target)
./scripts/run-tests.sh --timeout 180 t0000-basic.sh
./scripts/run-tests.sh t0000-basic.sh --quiet

# Regenerate docs/ dashboards after the run (off by default)
./scripts/run-tests.sh t0000-basic.sh --dashboard

# Isolated run: write results under another directory, leave data/tests/ and docs/ untouched
./scripts/run-tests.sh --data-dir /tmp/iso t0000-basic.sh
```

### Manually skipped files

Edit that test's TOML (**`data/tests/<group>/<stem>.toml`**): set **`in_scope = "skip"`**. Skipped files are **never** executed (single-file, group, or full run). Their tests are **excluded** from the summary counts on **`docs/progress/index.html`**. They still appear on **`docs/testfiles.html`** with a skipped badge so you can see what was opted out.

Re-run **`python3 scripts/generate-test-files-catalog.py`** if you add or rename `.sh` files and want the status tree updated without running tests (otherwise the next `run-tests.sh` also refreshes the catalog; TOMLs for deleted test files are pruned).

## Scripts reference

| Script                                          | Role                                                                                                             |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `scripts/test_status.py`                        | Shared helper: load/save **`data/tests/<group>/<stem>.toml`** files (atomic writes, TOML serialization, pruning). |
| `scripts/generate-test-files-catalog.py`        | Scan `tests/t*.sh`, merge the **`data/tests/`** tree (preserves `in_scope` and prior run results; prunes stale TOMLs). |
| `scripts/run-tests.sh`                          | Select files to run, execute harness, invoke apply (+ dashboard with `--dashboard`).                             |
| `scripts/apply-test-run-results.py`             | Merge one batch of run lines into the matching **`data/tests/`** TOMLs.                                          |
| `scripts/generate-dashboard-from-test-files.py` | Read `data/tests/` only; write **`docs/index.html`**, **`docs/progress/index.html`**, **`docs/testfiles.html`**, and **`docs/test-progress.svg`**. |

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
bare `cd`. Quick scan:
```bash
grep -c "test_expect_success" tests/tXXXX-*.sh   # total blocks
grep -cE "^[[:space:]]+cd [^&]*&&" tests/tXXXX-*.sh   # bare-cd blocks; ≈equal + low pass ⇒ this bug
```

## Dashboards

Not regenerated by default. Pass `--dashboard` to `run-tests.sh`, or refresh HTML only (no test run) with:

```bash
python3 scripts/generate-dashboard-from-test-files.py
```

## Other runners (not the data/tests pipeline)

These do **not** update `data/tests/` by default:

- **`scripts/run-upstream-tests.sh`** / **`scripts/aggregate-upstream.sh`** — run upstream `git/t/` against grit in isolation (see **AGENTS.md**).
- **`tests/harness/run-all-count.sh`** — separate harness; not wired to `data/tests/`.

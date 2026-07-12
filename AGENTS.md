---
description: 
alwaysApply: true
---

---
description: "Functionally complete Git reimplementation in idiomatic, library-focused Rust"
alwaysApply: true
---

# AGENTS.md — Working on Grit

A complete rewrite of Git in idiomatic, library-focused Rust code. This file is the durable build contract for autonomous runs.

## Product Intent

Grit is a from-scratch reimplementation of Git in idiomatic, library-oriented Rust.
The goal: pass the entire upstream Git test suite.

## Quick Start

```bash
# Build
cargo build --release -p grit-git

# Run a single test file
./scripts/run-tests.sh t3200-branch.sh

# Run one group (e.g. t1xxx)
./scripts/run-tests.sh t1

# Full harness (in-scope files only)
./scripts/run-tests.sh
```

## Testing pipeline (harness)

Upstream-style tests live in `tests/` and are driven by **`scripts/run-tests.sh`**. Per-file status and last-run counts live in per-test TOML files at **`data/tests/<group>/<stem>.toml`** (e.g. `data/tests/t0/t0000-basic.toml`). Dashboards **`docs/index.html`** (homepage progress section), **`docs/progress/index.html`**, **`docs/testfiles.html`**, and **`docs/test-progress.svg`** (README progress badge) are generated from that tree — only when you pass `--dashboard` to `run-tests.sh` or run `scripts/generate-dashboard-from-test-files.py` directly.

**Flow:**

1. **`scripts/run-tests.sh`** — Runs the requested files (single `.sh`, group prefix like `t1`, or all rows with `in_scope=yes`). Rows with **`in_scope=skip`** are never run.
2. **`scripts/generate-dashboard-from-test-files.py`** — Regenerates **`docs/index.html`** progress metrics, **`docs/progress/index.html`**, **`docs/testfiles.html`**, and **`docs/test-progress.svg`**.

To skip a file manually, set **`in_scope = "skip"`** in that test's TOML (`data/tests/<group>/<stem>.toml`). Skipped files are omitted from runs and from aggregate counts on the main dashboard.

Full detail: **TESTING.md**.

## The One Rule

**Fix grit Rust code to make upstream tests pass. Do not modify tests.**

The only exception: flipping `test_expect_failure` → `test_expect_success` when
you've fixed the underlying bug.

## Source of Truth

The canonical Git source code we're targeting to replicate the functionality of is in the `git/` subdirectory.

The tests we're trying to make pass with our new implementation are in the `git/t/` directory.

Manpage documentation is located in the `git/Documentation` directory as `*.doc` files.

## Licensing Hard Rule — No Copied Expression in `grit-lib`

`grit-lib` is **MIT-licensed**; Git's C source under `git/` is **GPLv2**. To keep
the library clean, this rule is absolute:

**Never copy protected expression from Git's C source into `grit-lib`. Use the
C source only for ideas, methods, interfaces, and behavior.**

- **Allowed** (not protectable): the *algorithm or method* (e.g. Myers diff, the
  approxidate parser, name-hash math), the *interface/behavior* it must produce,
  byte-for-byte *output compatibility*, and *facts* (keyword lists, opcode
  tables, format constants). Reimplement these in your own idiomatic Rust.
- **Forbidden** (protected expression copied verbatim or near-verbatim): Git's
  prose **comments**, multi-line **user-facing message strings**, and code whose
  **structure, naming, and layout** track the C beyond what the method requires.

If Git-identical user-facing text or other copied expression is genuinely
needed, it lives in the **`grit-git` CLI crate (`grit-git/src`), which is GPL-2.0** and
may reuse Git's strings and expression — not in `grit-lib`. Have the library
return a **structured/typed error or value**, and render the Git-compatible text
at the CLI boundary.

When porting from `git/`: read the C to understand *what* and *why*, then write
the Rust from that understanding — do not transcribe.

### Before Committing Rust Code

```bash
cargo fmt
cargo check # fix warnings
cargo clippy --fix --allow-dirty   # ensure no warnings remain
cargo test -p grit-lib --lib       # unit tests must pass
```

## Project Structure

```
grit/
├── grit-git/src/commands/     # Git command implementations
├── grit-lib/src/          # Core library (repo, index, diff, merge, etc.)
├── tests/                 # Ported upstream test files + test-lib.sh
├── git/t/                 # Upstream Git test suite (reference only)
├── data/tests/            # per-test status TOMLs (updated by run-tests.sh)
├── docs/                  # Dashboard HTML files
├── scripts/               # Test runner and dashboard generators
└── TESTING.md             # Full testing strategy
```

## Rust Style and Idioms

- Use traits for behaviour boundaries.
- Derive `Default` when all fields have sensible defaults.
- Use concrete types (`struct`/`enum`) over `serde_json::Value` wherever shape is known.
- **Match on types, never strings.** Only convert to strings at serialization/display boundaries.
- Prefer `From`/`Into`/`TryFrom`/`TryInto` over manual conversions. Ask before adding manual conversion paths.
- **Forbidden:** `Mutex<()>` / `Arc<Mutex<()>>` — mutex must guard actual state.
- Use `anyhow::Result` for app errors, `thiserror` for library errors. Propagate with `?`.
- **Never `.unwrap()`/`.expect()` in production.** Workspace lints deny these. Use `?`, `ok_or_else`, `unwrap_or_default`, `unwrap_or_else(|e| e.into_inner())` for locks.
- Prefer `Option<T>` over sentinel values.
- Use `time` crate (workspace dep) for date/time — no manual epoch math or magic constants like `86400`.
- Prefer guard clauses (early returns) over nested `if` blocks.
- Prefer iterators/combinators over manual loops. Use `Cow<'_, str>` when allocation is conditional.
- **No banner/separator comments.** Do not use decorative divider comments like `// ── Section ───`. Use normal `//` comments or doc comments to explain _why_, not to visually partition files.

## Dependencies

- **Do not use `gix` (gitoxide) or `git2` (libgit2).** This should be a clean reimplementation of Git and not rely on any other existing libraries.
- Do not ever shell out to the `git` binary. Everything should be reimplemented entirely in Rust.
- You may introduce any other stable Rust libraries that improve the process (such as for SHA1 hashing or command line parsing).

## Architecture and Design

- For code that you create, **always** include doc comments for all public functions, structs, enums, and methods and also document function parameters, return values, and errors.
- Documentation and comments **must** be kept up-to-date with code changes.
- Avoid implicitly using the current time like `std::time::SystemTime::now()`, instead pass the current time as argument.
- Keep public API surfaces small. Use `#[must_use]` where return values matter.
- Prefer implementing core Git behavior in `grit-lib` even when only one CLI command currently needs it. If code parses Git data, walks repository state, mutates objects/index/refs/worktrees, evaluates config semantics, formats Git-compatible records, or implements transport/protocol rules, it belongs in the library unless there is a clear CLI-only reason.
- Keep `grit-git/src` focused on argument parsing, environment/process setup, terminal/editor interaction, exit-code mapping, and converting library results into stdout/stderr. When adding command behavior, first design the typed library API, then call it from the CLI wrapper.
- Do not add reusable domain helpers under the binary crate as a staging area. If a helper would be useful to tests, another command, or a future embedding caller, add it to an appropriate `grit-lib/src` module with narrow visibility and lift to `pub` only as needed.

## Library Crate Layout and Public API

The Git-compatible engine should live in a **library crate** (`grit-lib`); the **`grit` binary** should stay a thin layer: parse CLI, open a `Repository` (or equivalent), call library APIs, map `grit_lib::Error` to exit codes and stderr. Agents should implement features in the library first and only wire them through the binary.

### When to use one crate vs several

- **Start with one library crate** plus the binary crate in a workspace unless a split is clearly needed. Prefer **modules** (`objects`, `index`, `refs`, `odb`, `tree`, `worktree`, …) for boundaries before adding more crates.
- **Split into additional library crates** when there is a stable boundary that yields real benefit: faster incremental builds for huge code, optional `#[cfg(feature = …)]` surfaces, or a subsystem that tests/tools want to depend on without pulling the whole repo stack. Avoid many tiny crates without a strong reason.
- **Integration tests** and future callers (benchmarks, fuzz targets) should depend on the **library**, not on private modules of the binary.

### What the library API should look like

- **Entry type:** Expose a single primary handle (e.g. `Repository`) obtained by opening a path or an explicit `GitDir` + work tree. Most operations are methods on that type or on focused borrows (`repo.index()`, `repo.odb()`) so callers do not thread global state.
- **Typed operations, not argv:** Public APIs take enums and newtypes (`ObjectId`, `RefName`, modes, tree entry kinds), not unparsed CLI strings. Parsing human-facing strings belongs at the CLI boundary.
- **Explicit context:** Time, randomness, and environment (e.g. `HOME`, config discovery) are **arguments or injectable providers**, not hidden `std::env` reads inside deep library calls—so the library stays testable and matches the rule against implicit "now" in core logic.
- **Errors:** Library uses **`thiserror`** enums with specific variants per failure mode; binary may wrap with `anyhow` for top-level reporting. Do not leak stringly "Git stderr" shapes from the library as the only error type.
- **IO boundaries:** Prefer passing `&mut dyn Read` / `Write` / `AsRef<Path>` where streams matter; for whole-repo operations, centralize filesystem access enough that tests can use temp dirs or in-memory backends without reimplementing commands.
- **Visibility:** Default to `pub(crate)` and lift to `pub` only when part of the supported API. Use `#[doc(hidden)]` sparingly for compatibility shims, not to hide a messy surface.
- **Stability mindset:** Treat the library as a long-lived API: avoid `pub` reexports of entire dependency modules; prefer small, documented extension points (traits) only where Git's own abstraction demands it.

### Traits and boundaries

- Use **traits** for behaviors that must vary (e.g. object storage backend, ref storage, optional fsmonitor-style hooks) or for non-consuming extension points—not for every struct.
- Keep "plumbing" operations as **coherent methods** on the appropriate type (`Index::write_tree`, `Odb::hash_object`) rather than a flat bag of free functions, unless a function group is truly stateless.

## Testing

- As tests in `git/t/` are being implemented, copy them to `./tests` and run them from there with `grit` aliased to `git` for the purposes of the tests.
- Do not write or run tests that are not from this directory.
- **Never run tests inside the main repo** — always use `/tmp/` scratch directories to avoid corrupting the working tree, index, or refs.
- Dashboards refresh automatically after `./scripts/run-tests.sh`; or run `python3 scripts/generate-dashboard-from-test-files.py`

## Do Not

- Modify `tests/test-lib.sh` (causes regressions)
- Create stub/partial test files (use full upstream tests)
- Skip tests by adding `SKIP` prereqs (fix the code instead)
- Run `cargo build` in worktrees (build in main repo, copy binary)

## Committing

This repository runs in **GitButler workspace mode** (the checked-out branch is `gitbutler/workspace`). Commit with the **`but` CLI**, not plain `git commit` — plain git commands that move HEAD will fight the workspace. `but` is a drop-in replacement for the git write workflows; read-only git (log, diff, blame) is fine.

Before committing, always run `cargo fmt` and `cargo clippy --fix --allow-dirty` and ensure no warnings remain (`cargo test -p grit-lib --lib` must pass).

## Cursor Cloud Specific Instructions

- **Rust toolchain**: The pre-installed Rust may be outdated. The update script runs `rustup update stable && rustup default stable` to ensure the latest stable toolchain is available, since newer workspace dependencies (e.g. `time-core`) require edition 2024 support (Rust ≥ 1.85).
- **No external services**: Grit is a pure CLI tool with no databases, containers, or network services. Build and test entirely via Cargo and the Bash test runner.
- **Unit tests**: Run `cargo test -p grit-lib --lib`. The `grit-git` crate has no lib target; use `cargo test --workspace` to run everything.
- **Integration tests**: Use `./scripts/run-tests.sh <test-file>` (see TESTING.md). Many tests are expected to fail — Grit is a work-in-progress.
- **Lint**: `cargo check -p grit-git 2>&1 | grep warning` — there are 2 pre-existing unused-variable warnings in `grit-git/src/commands/add.rs`.
- **Binary location**: After `cargo build --release`, the binary is at `target/release/grit-git`. The test harness expects this path.

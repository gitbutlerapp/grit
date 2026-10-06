---
description: 
alwaysApply: true
---

---
description: "Grit: fast linkable Git library, modern grit CLI, grit-git compatibility bed"
alwaysApply: true
---

# AGENTS.md — Working on Grit

Durable build contract for autonomous runs. Long-term plan: **ROADMAP.md** (snapshot; live plan on the [Grit Factory dashboard](https://maint.grit-scm.com/roadmap)).

## Project direction

Grit is a from-scratch Git engine in idiomatic Rust. Three deliverables:

| Piece | Role |
| ----- | ---- |
| **`grit-lib`** | Fast, clean, **linkable** library: typed APIs, no process globals, no stdout/stderr in library code. This is the product. |
| **`grit-cli`** | Modern **`grit`** client for daily workflows. **Every command** supports human output, **`--json`**, and **`--markdown`** (stable schemas). |
| **`grit-git`** | Git-compatible CLI (`grit-git` binary). Kept as a **compatibility and regression test bed**, not the primary UX goal. |

**What `grit-lib` must do.** Core Git semantics: object model and ODB (loose, pack, MIDX, alternates), refs and reflog, index and worktree, diff and merge, revwalk and rev-parse, config / ignore / attributes, transport and pack protocol, bundles, maintenance (gc, commit-graph, bitmaps). Pluggable ODB and ref backends over time. Behavior matches Git where compatibility matters; errors are **typed** in the library and rendered at CLI boundaries.

**What we drop.** Out-of-scope surface area leaves **`grit-lib`** (email/`am`/`send-email`, archive-only paths, SCM bridges, daemon/http-backend/instaweb, etc.). `grit-git` may keep thin shims; harness files for dropped commands are **`in_scope = "skip"`** with a reason (see ROADMAP item 5 and `docs/v1-scope.md`).

**Performance-first.** Measure before optimizing (`bench/`). Hot paths should track Git within roadmap targets; super-linear reloads (config, attributes, ignore in per-file loops) are bugs.

**Hashing.** One abstraction; **plain SHA-1 only — never sha1dc**. Prefer hardware acceleration (SHA-NI / aarch64 SHA extensions).

**Testing.** The upstream-style **harness is a regression gate** — in-scope pass counts must not regress. **New library behavior is tested in Rust** (integration tests on the public API; coverage tests for every public item). Convert core upstream cases from `git/t/` where they exercise library semantics; skip pure UX/flag-matrix tests. Details evolve in **TESTING.md** and ROADMAP testing items.

**Documentation.** Public API items get doc comments and examples. User-facing CLI and library docs stay in sync with each change (see ROADMAP item 3). Do not ship behavior without updating docs in the same change.

## Quick Start

```bash
# Build
cargo build --release -p grit-git
cargo build --release -p grit-cli

# Run a single harness file
./scripts/run-tests.sh t3200-branch.sh

# Run one group (e.g. t1xxx)
./scripts/run-tests.sh t1

# Full harness (in-scope files only)
./scripts/run-tests.sh
```

## Testing pipeline (harness)

Upstream-style tests live in `tests/` and are driven by **`scripts/run-tests.sh`**. Per-file status and last-run counts live in per-test TOML files at **`data/tests/<group>/<stem>.toml`**. Dashboards under **`docs/`** regenerate when you pass **`--dashboard`** to `run-tests.sh` or run `scripts/generate-dashboard-from-test-files.py`.

**Flow:**

1. **`scripts/run-tests.sh`** — Runs requested files. Rows with **`in_scope=skip`** are never run.
2. **`scripts/generate-dashboard-from-test-files.py`** — Refreshes progress dashboards from `data/tests/`.

To skip a file manually, set **`in_scope = "skip"`** in that test's TOML. Full detail: **TESTING.md**.

## Do not weaken tests

**Never weaken, delete, or skip tests to make a change pass.** The upstream harness **must not regress** on in-scope files you touch or that your change could affect.

Allowed: flip **`test_expect_failure` → `test_expect_success`** when the underlying bug is genuinely fixed.

New behavior belongs in **Rust tests** (`grit-lib` / workspace integration tests), not harness edits.

## Source of truth

- Reference implementation and manpages: **`git/`** (C source, **`git/t/`** tests, **`git/Documentation/*.doc`**).
- Ported harness copies: **`tests/`** (run against `grit-git` as `git`).

## Licensing hard rule — no copied expression in `grit-lib`

`grit-lib` is **MIT-licensed**; Git's C source under `git/` is **GPLv2**. To keep the library clean, this rule is absolute:

**Never copy protected expression from Git's C source into `grit-lib`. Use the C source only for ideas, methods, interfaces, and behavior.**

- **Allowed** (not protectable): the *algorithm or method* (e.g. Myers diff, the approxidate parser, name-hash math), the *interface/behavior* it must produce, byte-for-byte *output compatibility*, and *facts* (keyword lists, opcode tables, format constants). Reimplement these in your own idiomatic Rust.
- **Forbidden** (protected expression copied verbatim or near-verbatim): Git's prose **comments**, multi-line **user-facing message strings**, and code whose **structure, naming, and layout** track the C beyond what the method requires.

If Git-identical user-facing text or other copied expression is genuinely needed, it lives in the **`grit-git` CLI crate (`grit-git/src`), which is GPL-2.0** and may reuse Git's strings and expression — not in `grit-lib`. Have the library return a **structured/typed error or value**, and render the Git-compatible text at the CLI boundary.

When porting from `git/`: read the C to understand *what* and *why*, then write the Rust from that understanding — do not transcribe.

### Before committing Rust code

```bash
cargo fmt
cargo check # fix warnings
cargo clippy --fix --allow-dirty   # ensure no warnings remain
cargo test -p grit-lib --lib       # unit tests must pass
```

## Project structure

```
grit/
├── grit-lib/src/          # Core library (product)
├── grit-cli/src/          # Modern `grit` CLI (--json / --markdown)
├── grit-git/src/commands/ # Git-compatible CLI (compatibility bed)
├── tests/                 # Upstream-style harness + test-lib.sh
├── git/t/                 # Upstream Git tests (reference)
├── data/tests/            # Per-file harness status TOMLs
├── bench/                 # Benchmarks vs git
├── docs/                  # Site + harness dashboards
├── scripts/               # Test runner, dashboard generators
├── ROADMAP.md             # Ordered work plan (snapshot)
└── TESTING.md             # Harness + Rust test strategy
```

## Definition of done

Before calling work complete:

- [ ] **Rust tests** added or updated for new/changed library or CLI behavior.
- [ ] **Harness**: any in-scope files you touched still pass at least as well as before (no regressions).
- [ ] **Benchmarks**: for hot-path changes, before/after vs `git` recorded when applicable (`bench/`).
- [ ] **`grit-cli`**: new/changed commands expose **`--json`** and **`--markdown`** with stable shapes.
- [ ] **Docs** updated in the same change (rustdoc, TESTING.md mapping tables, user docs as they exist).
- [ ] **`cargo fmt`**, **`cargo clippy`** (no warnings), **`cargo test -p grit-lib --lib`** (and relevant workspace tests).

## Rust style and idioms

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
- Do not ever shell out to the `git` binary from **`grit-lib`** (CLI may delegate only where Git semantics require hooks/filters via injectable runners).
- You may introduce other stable Rust libraries that improve the process (hashing, compression, CLI parsing).

## Architecture and design

- For code that you create, **always** include doc comments for all public functions, structs, enums, and methods and also document function parameters, return values, and errors.
- Documentation and comments **must** be kept up-to-date with code changes.
- Avoid implicitly using the current time like `std::time::SystemTime::now()`; pass the current time as argument.
- Keep public API surfaces small. Use `#[must_use]` where return values matter.
- Prefer implementing core Git behavior in **`grit-lib`** even when only one CLI command currently needs it. If code parses Git data, walks repository state, mutates objects/index/refs/worktrees, evaluates config semantics, formats Git-compatible records, or implements transport/protocol rules, it belongs in the library unless there is a clear CLI-only reason.
- Keep **`grit-git`** and **`grit-cli`** focused on argument parsing, environment/process setup, terminal interaction, exit-code mapping, and converting library results into stdout/stderr (human / JSON / Markdown).
- Do not add reusable domain helpers under binary crates as a staging area. If a helper would be useful to tests, another command, or an embedding caller, add it to an appropriate **`grit-lib`** module with narrow visibility and lift to `pub` only as needed.

## Library crate layout and public API

The Git-compatible engine lives in **`grit-lib`**. Binaries stay thin: parse CLI, open a `Repository`, call library APIs, map `grit_lib::Error` to exit codes and output sinks.

### When to use one crate vs several

- **Start with one library crate** plus binary crates in a workspace unless a split is clearly needed. Prefer **modules** (`objects`, `index`, `refs`, `odb`, `tree`, `worktree`, …) for boundaries before adding more crates.
- **Split into additional library crates** when there is a stable boundary that yields real benefit: faster incremental builds, optional `#[cfg(feature = …)]` surfaces, or a subsystem that tests/tools want without pulling the whole repo stack.
- **Integration tests** and future callers (benchmarks, fuzz targets) should depend on the **library**, not on private modules of the binary.

### What the library API should look like

- **Entry type:** Expose a single primary handle (e.g. `Repository`) obtained by opening a path or an explicit `GitDir` + work tree. Most operations are methods on that type or on focused borrows (`repo.index()`, `repo.odb()`) so callers do not thread global state.
- **Typed operations, not argv:** Public APIs take enums and newtypes (`ObjectId`, `RefName`, modes, tree entry kinds), not unparsed CLI strings. Parsing human-facing strings belongs at the CLI boundary.
- **Explicit context:** Time, randomness, and environment (e.g. `HOME`, config discovery) are **arguments or injectable providers**, not hidden `std::env` reads inside deep library calls.
- **Errors:** Library uses **`thiserror`** enums with specific variants per failure mode; binaries may wrap with `anyhow` for top-level reporting. Do not leak stringly "Git stderr" shapes from the library as the only error type.
- **IO boundaries:** Prefer passing `&mut dyn Read` / `Write` / `AsRef<Path>` where streams matter; for whole-repo operations, centralize filesystem access enough that tests can use temp dirs or in-memory backends without reimplementing commands.
- **Visibility:** Default to `pub(crate)` and lift to `pub` only when part of the supported API. Use `#[doc(hidden)]` sparingly for compatibility shims, not to hide a messy surface.
- **Stability mindset:** Treat the library as a long-lived API: avoid `pub` reexports of entire dependency modules; prefer small, documented extension points (traits) only where Git's own abstraction demands it.

### Traits and boundaries

- Use **traits** for behaviors that must vary (e.g. object storage backend, ref storage, optional fsmonitor-style hooks) or for non-consuming extension points—not for every struct.
- Keep "plumbing" operations as **coherent methods** on the appropriate type (`Index::write_tree`, `Odb::hash_object`) rather than a flat bag of free functions, unless a function group is truly stateless.

## Testing (agents)

- Harness files under **`tests/`** are the **regression gate**; run affected files via **`./scripts/run-tests.sh`** before and after substantive changes.
- Add **Rust integration tests** against **`grit-lib`** for new semantics; add **coverage tests** for new public API surface.
- Do not write or run ad-hoc tests outside the repo's documented strategy.
- **Never run harness tests inside the main repo** — use `/tmp/` scratch directories.
- Dashboards: `./scripts/run-tests.sh --dashboard` or `python3 scripts/generate-dashboard-from-test-files.py`.

## Do not

- Modify `tests/test-lib.sh` (causes regressions)
- Create stub/partial harness files (use full upstream tests when porting)
- Skip harness tests by adding `SKIP` prereqs (fix the code instead)
- Weaken or delete tests to green a run
- Run `cargo build` in worktrees (build in main repo, copy binary)

## Committing

Agents version-control with **GitButler (`but`)** and **GitButler Mesh**. Nothing is pushed to GitHub for review except integration into **`origin/main`** with **`but merge`** after maintainer approval.

- Create work on **`factory/<short-slug>`** (or stacked branches as directed).
- Commit with **`but commit -m "…"`** — not plain **`git commit`** on the workspace branch.
- Read-only **`git`** (log, diff, blame) is fine.
- Before committing Rust: **`cargo fmt`**, **`cargo clippy --fix --allow-dirty`**, **`cargo test -p grit-lib --lib`**.

## Cursor cloud specific instructions

- **Rust toolchain**: Ensure stable ≥ 1.85 (`rustup update stable && rustup default stable`) for edition 2024 workspace deps.
- **No external services**: Build and test via Cargo and the Bash test runner.
- **Unit tests**: `cargo test -p grit-lib --lib`; use `cargo test --workspace` for broader runs.
- **Integration tests**: `./scripts/run-tests.sh <test-file>` (see TESTING.md). Many harness tests still fail overall; **do not regress** files in scope for your change.
- **Lint**: `cargo check -p grit-git 2>&1 | grep warning` — known pre-existing warnings in `grit-git/src/commands/add.rs`.
- **Binary location**: After `cargo build --release`, harness uses **`target/release/grit-git`**.

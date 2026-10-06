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

Grit began as a Rust reimplementation of Git aimed at passing Git's own test suite. It largely got there, but with workarounds, inefficient code, and support for every odd subcommand and legacy interface. **The project now has a new focus:**

| Crate | Role |
| ----- | ---- |
| **`grit-cli`** | Git client with a **modern CLI** (`grit` binary). Primary UX. |
| **`grit-lib`** | Clean, well-designed, **linkable** library any Rust project can use. |
| **`grit-git`** | Git-compatible CLI — **compatibility test bed** for the harness. |

Ordered work: **ROADMAP.md** (live plan on the [factory dashboard](https://maint.grit-scm.com/roadmap)).

### What `grit-lib` must do

Cover **most of what core Git does:**

- Object database: loose objects, packs, `.idx`, multi-pack-index, commit-graph
- Network push, fetch, and ls-remote
- Refs; the index; stash (read/write); reflog (read/write)
- Diffing, revwalking, revparsing
- Config (read/write); ignore rules; hooks; patch-ids; notes; bundles; fsck; worktrees

**Pluggable storage:** an ODB backend abstraction (like the one Git core is introducing); ref backends — loose, packed, and reftable.

On-disk formats and wire protocols must stay **correct and compatible with Git**.

### What we drop

Relatively unused commands get **no CLI and no further work:** archive; the email workflow (`am`, `format-patch`, `send-email`, `imap-send`, `request-pull`); foreign-VCS bridges. Remove from **`grit-lib`** where that simplifies the library. Mark related harness files **`in_scope = "skip"`** with a reason (ROADMAP item 5, `docs/v1-scope.md`).

### The CLI (`grit-cli`)

Every command implemented in **`grit-cli`** must be friendly for users, agents, and scripts:

- **Default** — clean, modern output for humans.
- **`--json`** — stable, documented, scripting-friendly output.
- **`--markdown`** — agent-friendly output.

### Performance is the top priority

The library and CLI must be **as fast as possible.**

- Benchmark widely; speed up everything you touch.
- Build and benchmark **real-world scenarios** (`bench/`): large repos, deep history, big packs, many refs, wide trees, network operations — no shortcuts that sacrifice compatibility.
- Compare core operations against the equivalent **Git core** commands.
- **Do not implement sha1dc.** Use plain SHA-1 and accelerate it (hardware SHA extensions, SIMD, parallel hashing).

### Testing

- Convert all **relevant** Git unit tests to Rust — core functionality and edge cases, **not** command UX or option compatibility.
- Write **coverage tests for every public library interface**.
- The upstream harness (`tests/*.sh` via **`grit-git`**) stays a **regression gate**. Do not weaken tests.

Detail: **TESTING.md** and ROADMAP testing items.

### Documentation

- Build and maintain a **documentation site** (usage docs + API guides); keep it up to date **with every change**.
- Include a **benchmarking section** comparing core functionality to equivalent Git core actions; regenerate as performance work lands.

### How work is judged

1. Correctness and compatibility of on-disk formats and wire protocols with Git.
2. Speed, measured.
3. Clean library API: typed errors, no stringly-typed matching, no printing from the library, no global state, no shelling out (in **`grit-lib`**).
4. Tests and docs land in the **same change** as the code.

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

`tests/` + **`scripts/run-tests.sh`**; per-file status in **`data/tests/<group>/<stem>.toml`**. **`in_scope=skip`** excludes a file. Dashboards: **`--dashboard`** or `scripts/generate-dashboard-from-test-files.py`. **TESTING.md** has full detail.

## Do not weaken tests

**Never weaken, delete, or skip tests to make a change pass.** The upstream harness **must not regress** on in-scope files you touch or that your change could affect.

Allowed: flip **`test_expect_failure` → `test_expect_success`** when the underlying bug is genuinely fixed.

New behavior belongs in **Rust tests** (`grit-lib` / workspace integration tests), not harness edits.

## Source of truth

- On-disk formats and wire protocols: the Git specification and observed compatibility with the **`git`** command where benchmarks and tests require it.
- Command behavior reference: [git-scm.com documentation](https://git-scm.com/docs) and the ported shell harness in **`tests/`** (run against `grit-git` as `git`).
- Library correctness: **`grit-lib`** Rust unit and integration tests (primary growth path).

## Licensing hard rule — no copied expression in `grit-lib`

`grit-lib` is **MIT-licensed**; Git's reference implementation is **GPLv2**. To keep the library clean, this rule is absolute:

**Never copy protected expression from Git's C sources into `grit-lib`. Use upstream Git only for ideas, methods, interfaces, and behavior.**

- **Allowed** (not protectable): the *algorithm or method* (e.g. Myers diff, the approxidate parser, name-hash math), the *interface/behavior* it must produce, byte-for-byte *output compatibility*, and *facts* (keyword lists, opcode tables, format constants). Reimplement these in your own idiomatic Rust.
- **Forbidden** (protected expression copied verbatim or near-verbatim): Git's prose **comments**, multi-line **user-facing message strings**, and code whose **structure, naming, and layout** track the C beyond what the method requires.

If Git-identical user-facing text or other copied expression is genuinely needed, it lives in the **`grit-git` CLI crate (`grit-git/src`), which is GPL-2.0** and may reuse Git's strings and expression — not in `grit-lib`. Have the library return a **structured/typed error or value**, and render the Git-compatible text at the CLI boundary.

When matching Git behavior: read published docs and specs for *what* and *why*, then write idiomatic Rust — do not transcribe GPL prose or code layout into **`grit-lib`**.

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
├── tests/                 # Shell harness + test-lib.sh (regression gate for grit-git)
├── data/tests/            # Per-file harness status TOMLs
├── bench/                 # Benchmarks vs git
├── docs/                  # Site + harness dashboards
├── scripts/               # Test runner, dashboard generators
├── ROADMAP.md             # Ordered work plan (snapshot)
└── TESTING.md             # Harness + Rust test strategy
```

## Definition of done

Aligns with **how work is judged** above:

- [ ] **Compatibility** — formats/protocols match Git; harness files you affect do not regress.
- [ ] **Speed** — benchmarks before/after vs `git` for hot-path or performance work (`bench/`).
- [ ] **Library hygiene** — typed errors, no lib printing/globals/shell-out; **`grit-cli`** adds **`--json`** and **`--markdown`** when touched.
- [ ] **Rust tests** + **coverage tests** for new/changed public API; relevant upstream cases converted over time.
- [ ] **Docs** (site + rustdoc) updated in the same change.
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
- Split crates only when a stable boundary clearly helps builds or optional surfaces. Integration tests depend on the **library**, not binary internals.

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

Run affected harness files before/after substantive changes. Add Rust integration + **coverage tests** for new **`grit-lib`** API. Never run harness inside the main repo — use `/tmp/`. See **Project direction → Testing** above.

## Do not

- Modify `tests/test-lib.sh` (causes regressions)
- Create stub/partial harness files (extend `tests/` with complete scenarios when adding coverage)
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

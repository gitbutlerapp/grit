---
description: "Grit: a fast, linkable Git library (grit-lib) and a modern Git client (grit-cli)"
alwaysApply: true
---

# AGENTS.md — Working on Grit

Durable build contract for autonomous runs. Long-term plan: **ROADMAP.md** (snapshot; live plan on the [Grit Factory dashboard](https://maint.grit-scm.com/roadmap)).

## Project direction

Grit began as a Rust reimplementation of Git aimed at passing Git's own test suite. It largely got there, but with workarounds, inefficient code, and support for every odd subcommand and legacy interface. That compatibility CLI (`grit-git`), the ported upstream shell harness, and the vendored Git source tree have been **removed**. **The project is now exactly two things:**

| Crate | Role |
| ----- | ---- |
| **`grit-lib`** | Clean, well-designed, **linkable** library any Rust project can use. **Where almost all implementation work belongs.** |
| **`grit-cli`** | Git client with a **modern CLI** (`grit` binary). Primary UX — **thin shell** over the library. |

Supporting crates exist only to serve those two: **`grit-protocol`** and **`grit-http-server`** (smart HTTP serving via `grit upload-pack` / `grit receive-pack`), **`grit-examples`** (small programs built on the library), **`grit-test-support`** (test helpers), and **`grit-utils`** (`grit-bench`). Do not add a Git-compatible command-line mirror back; Git compatibility means **on-disk formats and wire protocols**, not argv, messages or exit codes.

Ordered work: **ROADMAP.md** (live plan on the [factory dashboard](https://maint.grit-scm.com/roadmap)).

## Library-first (default for every task)

**For any task, the majority of the work — lines changed, tests written, and design effort — must land in `grit-lib` with a solid, linkable API.** The CLI crates exist to expose that API, not to host Git logic.

| Where effort should go | Typical share of a feature task |
| ---------------------- | --------------------------------- |
| **`grit-lib`** — behavior, types, errors, tests, rustdoc | **Most** (aim for the bulk of the diff) |
| **`grit-cli`** — clap parsing, exit codes, human / `--json` / `--markdown` output | **Minimal** |

**Default workflow:** design and implement in **`grit-lib`** first (or extend an existing type/method), add **Rust unit and coverage tests** there, document the **public API**, then wire **`grit-cli`** with the smallest possible glue.

**`grit-cli` (and the other binary crates) may only contain:**

- Argument and environment parsing
- Opening a `Repository` (or other library handle) and calling library APIs
- Mapping `grit_lib::Error` (and typed results) to exit codes and stdout/stderr
- Output formatting (default, `--json`, `--markdown`)

**Do not put in CLI crates:** object/index/ref/transport logic, diff or merge algorithms, config semantics, pathspec evaluation, or other reusable Git behavior — even if “only one command” needs it today. Add or extend **`grit-lib`** instead.

**Review heuristic:** if a change adds substantial non-presentation code under **`grit-cli/src`**, it is likely in the wrong crate. Move it to **`grit-lib`** and leave the CLI as a short call site.

See also **Architecture and design** and **Library crate layout and public API** below.

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

Relatively unused commands get **no CLI and no further work:** archive; the email workflow (`am`, `format-patch`, `send-email`, `imap-send`, `request-pull`); foreign-VCS bridges; `instaweb`, `daemon` and the other peripheral tools. Some `grit-lib` code still exists only because `grit-git` used it; remove it where that simplifies the library (ROADMAP item 5, `docs/v1-scope.md`).

### The CLI (`grit-cli`)

Every command implemented in **`grit-cli`** must be friendly for users, agents, and scripts:

- **Default** — clean, modern output for humans.
- **`--json`** — stable, documented, scripting-friendly output.
- **`--markdown`** — agent-friendly output.

### Performance is the top priority

The library and CLI must be **as fast as possible.**

- Benchmark widely; speed up everything you touch.
- Build and benchmark **real-world scenarios**: large repos, deep history, big packs, many refs, wide trees, network operations — no shortcuts that sacrifice compatibility. Benchmark **`grit-lib` operations and `grit` commands** (Criterion in `grit-lib`, `grit-bench` in `grit-utils`); the old `bench/` suite drove `grit-git` and was removed (ROADMAP item 2 rebuilds it).
- Compare core operations against the equivalent **Git core** commands.
- **Do not implement sha1dc.** Use plain SHA-1 and accelerate it (hardware SHA extensions, SIMD, parallel hashing).

### Testing

- Exercise core functionality and edge cases with **Rust tests** on the **`grit-lib`** public API (and workspace integration tests where CLI wiring matters). Cross-check against the system **`git`** binary inside tests where compatibility matters (formats, protocols, fsck).
- Write **coverage tests for every public library interface**.
- Do not weaken or delete tests to green a change.

Detail: **TESTING.md** and ROADMAP testing items.

### Documentation

- Build and maintain a **documentation site** (usage docs + API guides); keep it up to date **with every change**.
- **The CLI docs at [grit-scm.com/docs](https://grit-scm.com/docs/) must match `grit-cli`.** They are generated by `python3 scripts/docs.py` from `content/docs/` (`index.md` overview, `tutorial.md`, and one man page per command in `content/docs/commands/<command>.md`) into `docs/docs/`. Whenever you add, remove, rename or change a command, option, default, output or `--json` field in `grit-cli`, update that command's page (synopsis, description, options table, examples, JSON output) and the tutorial if it uses the command, then regenerate and commit both the Markdown and the generated HTML. Examples must be real output from running `grit`, not invented. The `every_command_is_documented` test in `grit-cli/src/main.rs` fails when a command has no page or a page misses one of its flags or subcommands.
- Include a **benchmarking section** comparing core functionality to equivalent Git core actions; regenerate as performance work lands.

### How work is judged

1. Correctness and compatibility of on-disk formats and wire protocols with Git.
2. Speed, measured.
3. **Library-first delivery:** behavior and API live in **`grit-lib`**; CLI crates stay thin (see **Library-first** above).
4. Clean library API: typed errors, no stringly-typed matching, no printing from the library, no global state, no shelling out (in **`grit-lib`**).
5. Tests and docs land in the **same change** as the code.

## Quick Start

```bash
# Build
cargo build --release -p grit-cli

# Test
cargo test -p grit-lib --lib
cargo test --workspace
```

## Testing

**TESTING.md** describes the strategy: `cargo test -p grit-lib --lib`, workspace integration tests (including the smart-HTTP transport tests served by `grit upload-pack` / `grit receive-pack`), and the GitHub Actions CI jobs with local reproduction commands.

## Source of truth

- On-disk formats and wire protocols: the Git specification (`gitformat-*`, `gitprotocol-*` on [git-scm.com](https://git-scm.com/docs)) and observed compatibility with the system **`git`** command in tests. The Git source tree is no longer vendored in this repository.
- Command behavior reference: [git-scm.com documentation](https://git-scm.com/docs) and **`grit-lib`** / workspace integration tests.
- Product APIs: `grit-lib` rustdoc and the docs site.

## Licensing hard rule — no copied expression in `grit-lib`

`grit-lib` is **MIT-licensed**; Git's reference implementation is **GPLv2**. To keep the library clean, this rule is absolute:

**Never copy protected expression from Git's C sources into `grit-lib`. Use upstream Git only for ideas, methods, interfaces, and behavior.**

- **Allowed** (not protectable): the *algorithm or method* (e.g. Myers diff, the approxidate parser, name-hash math), the *interface/behavior* it must produce, byte-for-byte *output compatibility*, and *facts* (keyword lists, opcode tables, format constants). Reimplement these in your own idiomatic Rust.
- **Forbidden** (protected expression copied verbatim or near-verbatim): Git's prose **comments**, multi-line **user-facing message strings**, and code whose **structure, naming, and layout** track the C beyond what the method requires.

The whole repository is **MIT**; there is no GPL crate to put Git's wording in. Have the library return a **structured/typed error or value**, and let `grit-cli` render it in **its own words**. Byte-for-byte machine formats (pack files, pkt-lines, protocol capability names) are facts and are fine.

When matching Git behavior: read published docs and specs for *what* and *why*, then write idiomatic Rust — do not transcribe GPL prose or code layout into **`grit-lib`**.

### Before committing Rust code

Format and fix issues in the code you touch, then run library tests:

```bash
cargo fmt
cargo check # fix warnings
cargo clippy --workspace -- -D warnings   # same gate as CI (optional: cargo clippy --fix --allow-dirty first)
cargo test -p grit-lib --lib       # unit tests must pass
```

Use **`cargo clippy --fix --allow-dirty`** on crates you change. The full workspace bar for integration is **`make gate`** (see **Committing** below).

## Project structure

```
grit/
├── grit-lib/src/          # Core library (product)
├── grit-cli/src/          # Modern `grit` CLI (--json / --markdown)
├── grit-protocol/         # Smart-protocol glue (spawns `grit upload-pack` / `receive-pack`)
├── grit-http-server/      # Smart HTTP server for serving repositories
├── grit-examples/         # Example programs built on grit-lib
├── grit-test-support/     # Shared test helpers
├── grit-utils/            # grit-bench and other maintenance tools
├── content/blog/          # Blog sources (rendered by scripts/blog.py)
├── content/docs/          # CLI tutorial + man pages (rendered by scripts/docs.py)
├── docs/                  # Site + usage docs
├── scripts/               # Repo maintenance scripts
├── ROADMAP.md             # Ordered work plan (snapshot)
└── TESTING.md             # Rust test strategy
```

## Definition of done

Aligns with **how work is judged** above:

- [ ] **Library-first** — new behavior and tests are in **`grit-lib`**; **`grit-cli`** changes are mostly wiring and output.
- [ ] **Compatibility** — formats/protocols match Git where implemented; Rust tests cover the change.
- [ ] **Speed** — benchmarks before/after vs `git` for hot-path or performance work.
- [ ] **Library hygiene** — typed errors, no lib printing/globals/shell-out; **`grit-cli`** adds **`--json`** and **`--markdown`** when touched.
- [ ] **Rust tests** + **coverage tests** for new/changed public API.
- [ ] **Docs** (site + rustdoc) updated in the same change; CLI changes update `content/docs/` and rerun `python3 scripts/docs.py`.
- [ ] **`make gate`** passes (or the same checks: fmt, **`cargo clippy --workspace -- -D warnings`**, **`cargo test --workspace`**).

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
- Keep **`grit-cli`** focused on argument parsing, environment/process setup, terminal interaction, exit-code mapping, and converting library results into stdout/stderr (human / JSON / Markdown).
- Do not add reusable domain helpers under binary crates as a staging area. If a helper would be useful to tests, another command, or an embedding caller, add it to an appropriate **`grit-lib`** module with narrow visibility and lift to `pub` only as needed.

## Library crate layout and public API

The Git engine lives in **`grit-lib`**. Binaries stay thin: parse CLI, open a `Repository`, call library APIs, map `grit_lib::Error` to exit codes and output sinks.

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

Run **`cargo test -p grit-lib --lib`** and relevant workspace tests before/after substantive changes. Add integration + **coverage tests** for new **`grit-lib`** API. For hot paths, benchmark against system `git`.

## Do not

- Weaken or delete tests to green a run
- Reintroduce a Git-compatible CLI mirror, a ported upstream shell harness, or a vendored copy of Git's source
- Run `cargo build` in worktrees (build in main repo, copy binary)

## Committing

Agents version-control with **GitButler (`but`)** and **GitButler Mesh**. Nothing is pushed to GitHub for review except integration into **`origin/main`** with **`but merge`** after maintainer approval.

- Create work on **`factory/<short-slug>`** (or stacked branches as directed).
- Commit with **`but commit -m "…"`** — not plain **`git commit`** on the workspace branch.
- Read-only **`git`** (log, diff, blame) is fine.
- Before committing Rust: **`cargo fmt`**, **`cargo test -p grit-lib --lib`** (and clippy on crates you touch).

**Pre-integration gate:** before **`but merge`**, rebase onto **`origin/main`**, run **`make gate`** on the rebased branch, and merge only if it exits **0**. See **`TESTING.md`** (pre-integration gate and integration procedure).

## Cursor cloud specific instructions

- **Rust toolchain**: Ensure stable ≥ 1.85 (`rustup update stable && rustup default stable`) for edition 2024 workspace deps.
- **No external services**: Build and test via Cargo.
- **Pre-integration gate**: **`make gate`** (`scripts/gate.sh`: fmt, clippy with **`-D warnings`**, **`cargo test --workspace`**).
- **Unit tests**: `cargo test -p grit-lib --lib` during development; the gate runs the full workspace suite.
- **Lint**: `cargo clippy --workspace -- -D warnings` must pass; warnings fail CI (see **TESTING.md**).
- **Benchmarks**: when touching hot paths, compare against system `git` (Criterion in `grit-lib`, `grit-bench`).

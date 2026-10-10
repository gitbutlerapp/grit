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

Build and maintain the **documentation site** (usage docs, library guide, benchmarks) and **`grit-lib` rustdoc**; ship doc updates in the **same change** as the code they describe. Published CLI and guide pages live at [grit-scm.com/docs](https://grit-scm.com/docs/).

**How the site works.** [grit-scm.com](https://grit-scm.com) is a Next.js app in **`site/`**, deployed to Vercel on every push to `main` by the [`Site`](.github/workflows/site.yml) workflow. It reads the Markdown in **`content/docs/`** and **`content/blog/`** at build time. **There is no generated HTML to commit and nothing to regenerate:** edit the Markdown (and code) in your change, and the deploy picks it up. Everything derived is built for you:

- **CLI docs** come straight from `content/docs/commands/<cmd>.md`.
- **Library docs** pull compiled examples from **`grit-examples`** (`<!-- include: grit-examples/... -->`), resolve **`rustdoc:grit_lib::…`** links against fresh rustdoc, and generate the **API map** from rustdoc, so a renamed or removed public item fails the build instead of leaving a dead link.
- **Benchmarks** tables are generated from the committed **`grit-bench`** baselines.
- **Agent outputs** (each page's `index.md` twin, `/llms.txt`, `/llms-full.txt`, `/docs/grit-cli.md`, `/docs/grit-lib.md`) and **share cards** are built from the same sources.

| Command | What it does |
| --- | --- |
| **`make docs`** | Preview the site at http://localhost:3000 (builds `grit-lib` rustdoc, then `npm run dev` in `site/`). Edits to `content/` show up on reload. |
| **`make docs-check`** | Same as the CI **Site** workflow: content check (manifest, code fence languages, includes, `rustdoc:` links, internal links and anchors), site tests, type check, production build. |
| **`make doc`** | Workspace API reference with **`RUSTDOCFLAGS="-D warnings"`** (same as CI **rustdoc** job and the rustdoc stage of **`make gate`**). |

Quick check while editing docs: **`cd site && npm run check`** (seconds; needs `cargo doc -p grit-lib --no-deps` once for `rustdoc:` links). Site code (layouts, styles) lives in `site/app`, `site/components` and `site/lib`; agents normally only touch `content/`.

#### Adding docs for a change

| If you changed… | Update |
| --- | --- |
| A **`grit` command**, flag, default, human/`--markdown` output, or **`--json` field** | [`content/docs/commands/<cmd>.md`](content/docs/commands/) following [`content/docs/commands/README.md`](content/docs/commands/README.md) (real terminal and JSON examples, JSON field table). Update [`content/docs/tutorial.md`](content/docs/tutorial.md) when the tutorial uses the command. |
| A **public `grit-lib` API** (types, methods, behavior) | Rustdoc on the item (the API map and `rustdoc:` links follow automatically). For guided workflows, add or edit a page under [`content/docs/library/`](content/docs/library/) and/or a compiled example in **`grit-examples`** referenced with `<!-- include: grit-examples/... -->` (see [`content/docs/library-quickstart.md`](content/docs/library-quickstart.md)). When you rename or remove an item, fix the `rustdoc:` links `npm run check` reports. |
| **Performance** (Criterion, hot paths, or **`grit-bench`** scenarios) | Refresh committed baseline JSON per **ROADMAP.md** item 2 (`grit-bench … --format json --output grit-utils/baselines/…`). List paths in **`[benchmarks].baseline`** in [`content/docs/site.toml`](content/docs/site.toml). Edit prose in [`content/docs/benchmarks.md`](content/docs/benchmarks.md) outside `<!-- grit:benchmark-tables -->` when methodology or narrative changes. |
| A **new site page** (guide, topic, or command) | Register it in [`content/docs/site.toml`](content/docs/site.toml) (`[[section.page]]`, `[section.commands]`, or `directory = "library"`). Every `.md` under `content/docs/` must appear in the manifest. |
| A **blog post** | Add `content/blog/<slug>.md` with `title`, `date` (`YYYY-MM-DD`) and `summary` front matter. |

Every code fence needs a language (`console` for terminal sessions with `$` prompts, `bash` for scripts, `rust`, `json`, `toml`, `text`): the site lays out and highlights code by language.

**What CI enforces**

- **`Site` workflow** ([`site.yml`](.github/workflows/site.yml)) — on PRs and on `main` pushes touching `content/`, `site/`, `grit-examples/`, `grit-lib/` or `grit-utils/baselines/`: **`make docs-check`** equivalents in strict mode, then (on `main`) the production deploy. A failing check means the live site stays on the last good deploy, so fix it promptly.
- **`rustdoc` job** — workspace **`cargo doc`** with **`-D warnings`** (**`make doc`**).
- **`test` job** — **`every_command_is_documented`** in `grit-cli/src/main.rs` (every clap flag and subcommand is mentioned on its command page).

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
├── content/blog/          # Blog posts (Markdown)
├── content/docs/          # Docs sources (site.toml, commands/, library/)
├── site/                  # grit-scm.com: Next.js app built from content/ (deployed to Vercel)
├── docs/                  # Retired GitHub Pages copy of the old site; do not edit (removed after the DNS move)
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
- [ ] **Docs** — follow **Adding docs for a change** above; **`cd site && npm run check`** passes (or **`make docs-check`** for the full CI equivalent); **`make doc`** when rustdoc changed.
- [ ] **`make gate`** passes (fmt, clippy **`-D warnings`**, workspace rustdoc **`-D warnings`**, **`cargo test --workspace`** — see **TESTING.md**).

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

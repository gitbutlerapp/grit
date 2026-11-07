# grit-lib public API

This document describes how the **supported** surface of `grit-lib` is shaped and how it may change on the path to **0.6.0** and afterward. It complements the [API layout guide](https://grit-scm.com/docs/library/api-layout) (canonical module paths) and the [rustdoc](https://docs.rs/grit-lib) generated from source.

## Conventions

These rules apply to every public item in `grit-lib`. They are the bar for reaching **0.6.0** with an API embedders can rely on.

### 1. One entry point

[`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) is the primary handle for an on-disk or injected repository. Long-lived subsystems are exposed as **focused handles** obtained from it (for example objects, references, the index, configuration, revision walking, and diffs), not as free functions that take `&Repository` or a bare git-dir path.

Pure helpers (parsing, formatting, hashing) may remain free functions when they do not need repository state.

### 2. Options structs, not variants

Optional behavior belongs in a single `#[non_exhaustive]` options struct with `Default` and builder-style setters on that struct. Avoid:

- `_with_x_and_y` function families
- More than one bare `bool` parameter on a public function (prefer an enum or the options struct)
- `#[allow(clippy::too_many_arguments)]` on public functions

### 3. One type per concept

Use one cancellation type, one progress sink, [`RefName`](https://docs.rs/grit-lib/latest/grit_lib/refs/check_ref_format/struct.RefName.html) (or equivalent) for ref names, a dedicated pathspec type instead of `&[String]`, and configuration through `repo.config()` rather than ad hoc config reads deep in call stacks.

### 4. Typed errors per area

Each functional area exposes its own `#[non_exhaustive]` `thiserror` enum with structured fields. The public API must not use a catch-all `Message(String)` variant or `Result<_, String>` as the primary error shape.

**Area map** (one umbrella variant and enum per row; fold one-off variants into the neighbour — see [library/errors guide](https://grit-scm.com/docs/library/errors)):

| Area | Enum | Module home |
| --- | --- | --- |
| Repository | `RepositoryError` | `repo` |
| Config | `ConfigError` | `config` |
| Hook | `HookError` | `command_runner` / hooks |
| Object | `ObjectError` | `objects`, maintenance, fsck |
| Ref | `RefError` | `refs`, reflog, reftable |
| Branch / Tag / Notes | `BranchError`, `TagError`, `NotesError` | porcelain + `refs` |
| Index | `IndexError` | `index` |
| Worktree | `WorktreeError` | `worktree`, pathspec, filters |
| Diff | `DiffError` | `diff`, blame, apply, rerere |
| Revision | `RevisionError` | `revwalk`, merge-base |
| Commit | `CommitError` | `commit`, signing |
| Merge | `MergeError` | merge, pick, revert, `operation` |
| History | `HistoryError` | `revwalk::history` |
| Stash | `StashError` | `stash` |
| Remote | `RemoteError` (wraps `BundleError`, `CloneError`, `ServeError`, …) | `remote`, `bundle`, clone, serve |

**Contract rules**

- `#[non_exhaustive]` `thiserror` on every public `*Error` enum; structured named fields (`PathBuf`, ids, ref names, reason enums); `#[source]` for causes.
- No tuple variant whose only payload is `String`; no area-level catch-alls. `String` fields only for external text (for example remote stderr), always named.
- The umbrella `Error` has exactly one variant per area (plus `Io`, `LockHeld` and `Cancelled`); `grit-lib/src/error/mod.rs` pins this with `umbrella_error_has_one_variant_per_area`. Operations return `grit_lib::error::Result<T>`; area enums convert into the umbrella with `From` (no `type FooResult = Result<…>` aliases in the public API).
- Area enums never hold an umbrella `Error` (ratchet `pub_area_error_wraps_umbrella` is zero): a lower area's failure surfaces as its own umbrella variant. A variant that must also report a secondary failure (rollback, per-remote failures) keeps an [`ErrorSummary`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.ErrorSummary.html).
- Path I/O: [`PathIoError`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.PathIoError.html) everywhere a path is involved — not bare `#[from] io::Error` on area enums.
- Shared leaves: [`LockHeldError`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.LockHeldError.html), [`PathIoError`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.PathIoError.html), [`Cancelled`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.Cancelled.html), [`ErrorSummary`](https://docs.rs/grit-lib/latest/grit_lib/error/struct.ErrorSummary.html).
- CLI-only clashes do not belong in the library.
- Tests assert on **variants**, not message strings. Pin `kind_name` slugs in `grit-lib/tests/error_contract.rs` (legacy slugs plus every area table's slugs).

### 5. Typed outcomes

Operations return structured results (counts, classifications, names, conflict state) so callers and the CLI do not re-derive the same facts from logs or side effects.

### 6. Public means supported

- Default to `pub(crate)`; lift to `pub` only for the supported API.
- One public path per concept (see [API layout](https://grit-scm.com/docs/library/api-layout)).
- Nothing named for tests, probes, fixtures, or benchmarks is public; helpers for those live behind the **`test-support`** cargo feature.
- No `libc` types and no `pub unsafe fn` in the public API.

### 7. Documented

Every public item has rustdoc. Entry points include examples. Crate-level documentation states the current release line (major.minor) and links here for stability policy.

## Canonical forms

Each public operation should end up in one of the shapes below. The API 0.6 steps move every operation into one of them, one area at a time. To list what is left in an area, run `make api-ratchet-report` (or `make api-ratchet-report MODULE=diff`, see [Ratchet report](#ratchet-report)).

### Handle map

An operation on a repository is a method on the handle that owns its area. Extend an existing handle before adding a new accessor; a new accessor is only for an area no handle owns yet.

| Area | Accessor | Handle type | Owns |
| --- | --- | --- | --- |
| Objects | `repo.objects()` | `objects::Objects` | Object read/write, tree and commit building, upload-pack streaming, promisor fetches |
| Maintenance | `repo.maintenance()` | `maintenance::Maintenance` | gc, `gc --auto`, repack, prune, commit-graph and MIDX writes, pack-refs, offload and drop of large blobs, fsck, the `gc.pid` lock |
| References | `repo.references()` | `refs::References` | Resolve, list and update refs; `HEAD` and symbolic refs; short-name expansion; branches and tags; ref transactions |
| Reflogs | `repo.reflogs()` | `repo_handles::RepositoryReflogs` | Reflog read, append, expire |
| Operation state | `repo.operation()` | `operation::Operations` | Paused merge, rebase, pick, revert and bisect state (`MERGE_HEAD`, `ORIG_HEAD`, sequencer), what blocks a switch, forgetting paused state |
| Index and staging | `repo.index()` | `index::IndexFile` | Index load/write (`IndexLoadOptions`, `IndexWriteOptions`); add, rm and mv; sparse checkout |
| Config | `repo.config()` | `config::Config` | Config cascade reads, edits and typed values |
| Revisions | `repo.revwalk()` | `revwalk::RevWalk` | Rev-parse, rev-list, merge-base, log walks, ahead/behind |
| Diff | `repo.diff()` | `diff::Diff` | Tree, index and work-tree diffs; text rendering; blame; patch-ids; apply |
| Remotes and transport | `repo.remotes()` | `repo_handles::RepositoryRemotes` | Remote config, fetch, push, ls-remote, serving, snapshot checkouts |
| Bundles | `repo.bundles()` | `repo_handles::RepositoryBundles` | Bundle create, verify, unbundle |
| Notes | `repo.notes()` | `repo_handles::RepositoryNotes` | Notes read, add, remove, rewrite |
| Stash | `repo.stash()` | `repo_handles::RepositoryStash` | Stash push, apply, pop, drop, list |
| Hooks | `repo.hooks()` | `repo_handles::RepositoryHooks` | Hook lookup and dispatch |
| Linked worktrees | `repo.worktrees()` | `repo_handles::RepositoryWorktrees` | Add, list, lock, move, remove and prune linked worktrees |
| Workspace snapshots | `repo.workspace_snapshot()` | `repo_handles::RepositoryWorkspaceSnapshot` | GitButler workspace snapshots |
| Current checkout (new) | `repo.checkout()` | *to be added* | Status, switch, checkout, reset, restore, clean |
| Merge family (new) | `repo.merge()` | *to be added* | Merge, pick, revert, replay, `--check` predictions, `--continue`, `--abort` and `--skip` (reading paused state stays on `operation()`) |
| History | `repo.history()` | `revwalk::history::History` | Commit creation and amend, history edits (reword, drop, move, fixup, squash, split, absorb), undo, rebase sessions |

The remaining new accessors are added by the first step that needs each one. `repo.work_tree()` already returns the work-tree path, so the current-checkout handle is `checkout()`. Linked-worktree administration stays on `worktrees()`.

### Options structs

Optional behavior goes in one options struct per operation (an illustrative example):

```rust
/// Options for repacking the object database.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RepackOptions {
    /// Which objects end up in the new pack.
    pub scope: RepackScope,
    /// Pack window size; `None` uses `pack.window`.
    pub window: Option<u32>,
}

impl RepackOptions {
    /// Sets [`scope`](Self::scope).
    #[must_use]
    pub fn with_scope(mut self, scope: RepackScope) -> Self {
        self.scope = scope;
        self
    }

    /// Sets [`window`](Self::window).
    #[must_use]
    pub fn with_window(mut self, window: Option<u32>) -> Self {
        self.window = window;
        self
    }
}

/// Which objects a repack writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RepackScope {
    /// Only loose objects and packs not yet in the main pack.
    #[default]
    Incremental,
    /// Everything reachable, into one pack.
    All,
}
```

- `#[non_exhaustive]` and `Default`, so new fields are additive and callers write `RepackOptions::default().with_window(Some(50))`.
- One `with_<field>` setter per field. These setters are the only `with_` names in the API. A `_with_` after a prefix (`diff_trees_with_renames`) marks a variant to fold into an options struct, and the `pub_fn_with_in_name` ratchet counts it. Legitimate names that contain `_with_` are renamed, with no exemption list.
- Use enums instead of bools whenever a choice has a name (`RepackScope::All`, not `all: bool`). One bare `bool` parameter is the most a public function takes.
- The options struct is the last value parameter: `repo.maintenance().repack(&RepackOptions::default(), progress, cancel)`.

### Progress and cancellation

Long-running operations take progress and cancellation as **trailing parameters**, in this order:

```rust
pub fn repack(
    &self,
    options: &RepackOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<RepackOutcome, ObjectError>
```

[`ProgressSink`](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.ProgressSink.html) and [`CancelToken`](https://docs.rs/grit-lib/latest/grit_lib/progress/struct.CancelToken.html) are the only progress and cancel types. Callers that don't care pass `&mut NullProgress` and `&CancelToken::never()`. There are no `_with_progress` / `_with_cancel` copies of an operation, and no progress or cancel fields on options structs.

### Free functions

A free function is only for a **pure computation on values**: parsing, formatting, hashing, or validating bytes and strings the caller already has. A function whose first parameter is `&Repository`, or a git-dir `&Path` such as `repo.git_dir()`, is an operation on a repository and becomes a method on the owning handle. The `pub_free_fn_repository_first` and `pub_free_fn_git_dir_path_first` ratchet metrics count the ones that remain.

Rustdoc examples on the crate root and on handle types show the handle form. The `doc_example_free_fn_calls` metric counts calls in those examples to repository free functions (for example `resolve_head(repo.git_dir())`). It may only go down.

### Superseded functions

When a handle method or options struct replaces a function:

- If internal hot paths still call the old function, it becomes `pub(crate)` and stays as the implementation.
- Otherwise it is deleted.
- Before 0.6.0, neither case gets a deprecated public shim. From 0.6.0 the [stability policy](#stability-policy) applies.
- Measure before and after. A consolidation may not make anything slower.

### Errors pass through

Consolidation steps keep each operation's error type unchanged: the new method returns the same `Result<_, E>` as the function it replaces. Typed per-area errors (convention 4) are a separate roadmap item. Don't change error types during a move.

### Ratchet report

```bash
make api-ratchet-report             # every top-level module, largest first
make api-ratchet-report MODULE=diff # one module
python3 scripts/api_ratchet.py report --module porcelain
```

The report lists the offenders for `pub_fn_with_in_name`, `pub_fn_too_many_arguments_allow`, `pub_fn_two_plus_bare_bool`, `pub_free_fn_repository_first` and `pub_free_fn_git_dir_path_first`. It groups them by the top-level public module a caller imports them from. Items defined in private implementation modules (`history_root`, `rev_parse`, …) are grouped under the public module that re-exports them. The totals match `make api-ratchet-check`.

## Stability policy

**Before 0.6.0:** breaking changes are acceptable when they **consolidate** the API (fewer, clearer ways to do each thing, following the conventions above). Do not break the public API for unrelated refactors.

**From 0.6.0 through 0.6.x:** the public API only **grows**. No removals, renames, or signature changes without keeping the old form: add the replacement, mark the old entry `#[deprecated(since = "0.6.x", note = "use …")]`, and remove it in **0.7**.

**Enforcement:** CI runs [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks) against the git revision pinned in [`api-ratchet.toml`](api-ratchet.toml) (`semver_baseline`) until **0.6.0**, then against the latest reachable `v*` tag (see [Gates](#gates)). The committed [`public-api.txt`](public-api.txt) snapshot and quality counts in the same ratchet file complement that check.

## How to add a public item

Use this checklist before merging a new or widened public API:

1. **Placement** — Does the operation belong on the right handle (`Repository` or a focused borrow) rather than as a crate-root free function?
2. **Options** — If behavior is configurable, add or extend a `#[non_exhaustive]` options struct with `Default` and setters.
3. **Errors** — Extend the area’s `thiserror` enum (or add a new one); no stringly errors.
4. **Documentation** — Rustdoc on the item, with an example for entry-level APIs.
5. **Tests** — Unit tests in `grit-lib` and, for public surface, an integration or coverage test that exercises the API path.
6. **Snapshot** — Run `make public-api` when the intentional public surface changes and commit the updated `grit-lib/public-api.txt`.
7. **Ratchet** — When the API count ratchet is enabled, update its baseline if your change is approved (see [Gates](#gates)).
8. **Docs site** — For user-visible or guide-worthy APIs, add or update a page under `content/docs/library/` and ensure `cd site && npm run check` passes.

## Audit

Snapshot date: **2026-11-05**, from committed [`public-api.txt`](public-api.txt) (**27 980** lines; `wc -l` on the file). Top-level modules match:

```bash
grep -E '^pub mod grit_lib::[a-z_0-9]+$' grit-lib/public-api.txt
```

**External users** counts are the number of files under each consumer tree that reference `grit_lib::<module>` (or `grit_lib::<module>::`), counted with `rg -l`. **Ratchet** columns use approximate per-module counts from `public-api.txt`: free functions whose first parameter is `&Repository` / `Repository`, and public functions whose name contains `_with_`. Workspace totals from [`api-ratchet.toml`](api-ratchet.toml) (rustdoc JSON via `make api-ratchet-check`) are **385** repo-first and **112** `_with_`-style function names; the baseline records **27 980** `public_api_lines`, matching the committed snapshot file.

### Regenerating counts

For current per-module offender lists (from rustdoc JSON, matching the ratchet totals), use `make api-ratchet-report`; the script below reproduces this audit's `public-api.txt` approximation.

From the repository root:

```bash
# Module list + line spans + repo-first / _with_ per top-level module
python3 - <<'PY'
import re, subprocess
from pathlib import Path
lines = Path("grit-lib/public-api.txt").read_text().splitlines()
mod_re = re.compile(r"^pub mod grit_lib::([a-z_0-9]+)$")
fn_re = re.compile(r"^pub fn grit_lib::([a-z_0-9]+)::([^(\s]+)\(")
repo_re = re.compile(
    r"^pub fn grit_lib::([a-z_0-9]+)::[^(\s]+\((&grit_lib::repo::Repository|grit_lib::repo::Repository)"
)
mods = [(m.group(1), i) for i, ln in enumerate(lines) if (m := mod_re.match(ln))]
paths = (
    "grit-cli", "grit-utils", "grit-examples", "grit-protocol",
    "grit-http-server", "grit-test-support", "grit-lib/tests",
)
for j, (name, start) in enumerate(mods):
    end = mods[j + 1][1] if j + 1 < len(mods) else len(lines)
    rf = wi = 0
    for ln in lines[start:end]:
        m = fn_re.match(ln)
        if not m or m.group(1) != name:
            continue
        if repo_re.match(ln):
            rf += 1
        if "_with_" in m.group(2):
            wi += 1
    files = set()
    for p in paths:
        r = subprocess.run(["rg", "-l", f"grit_lib::{name}", p], capture_output=True, text=True)
        if r.returncode == 0:
            files.update(f for f in r.stdout.splitlines() if f)
    print(f"{name}\t{end - start}\t{rf}\t{wi}\t{len(files)}")
PY
```

Handle `.accessor()` file counts: `rg -l '\\.<accessor>\\(' grit-cli grit-utils grit-examples grit-protocol grit-http-server grit-test-support grit-lib/tests | wc -l`.

Decisions are **keep**, **merge into …** (executed in a later roadmap step), or **make private**. No merge row is owned by this audit-only step.

### Table 1 — public modules and root re-exports

| Path | Lines | External users | Ratchet (repo-first / `_with_`) | Decision | Owner | Why |
| --- | ---: | --- | --- | --- | --- | --- |
| `apply` | 219 | 13 (cli 2, utils 1, tests 10) | 2 / 0 | keep | — | Patch parse/apply types and index application; core library surface for `grit apply`. |
| `bench_fixture` | 137 | 7 (utils 6, tests 1) | 0 / 1 | merge into `objects::bench_fixture` | Test-only items out of the public API | Root alias duplicates canonical bench helpers; remove from supported API alongside nested fixture surface. |
| `blame` | 337 | 25 (cli 1, utils 2, tests 22) | 5 / 1 | keep | Visibility sweep: revwalk, diff, porcelain, commit, blame, apply | Blame options and results belong on the diff/rev domain; shrink nested helpers in the sweep. |
| `bundle` | 151 | 15 (cli 1, utils 1, examples 1, http 3, tests 9) | 11 / 9 | keep | Visibility sweep: objects, maintenance, bundle | Bundle read/write and remote bundle transport; nested `bundle_remote` re-exports trimmed to consumer paths. |
| `change_id` | 135 | 19 (cli 9, tests 10) | 2 / 0 | keep | — | Change-id header generation is a first-class cross-cutting feature. |
| `command_runner` | 497 | 29 (cli 7, examples 2, tests 20) | 10 / 1 | keep | — | Injectable subprocess/hook runner boundary for CLI and tests. |
| `commit` | 282 | 19 (cli 4, examples 1, tests 14) | 0 / 1 | keep | Visibility sweep: revwalk, diff, porcelain, commit, blame, apply | Commit encoding, signing, and trailer parsing for porcelain and hooks. |
| `config` | 1204 | 688 (cli 42, utils 6, examples 48, protocol 6, http 2, ts 1, tests 583) | 4 / 11 | keep | One path for pathspec and ident | Config cascade is central; nested `ident_*` and date helpers consolidate under `config()` / `ident`. |
| `diagnostics` | 132 | 16 (cli 1, examples 1, tests 14) | 0 / 0 | keep | — | Structured diagnostics sink wired from `Repository::diagnostics`. |
| `diff` | 2378 | 181 (cli 26, utils 3, examples 7, tests 145) | 20 / 25 | keep | Visibility sweep: revwalk, diff, porcelain, commit, blame, apply | Tree/content diff, merge, rerere, and patch-id helpers. |
| `divergence` | 230 | 2 (cli 1, tests 1) | 7 / 0 | keep | — | Ahead/behind and divergence summaries for `grit shortlog` / `grit ahead`. |
| `error` | 1173 | 369 (cli 36, utils 2, examples 44, protocol 1, http 2, ts 2, tests 282) | 0 / 0 | keep | Typed, per-area errors | Legacy umbrella `Error` shrinks as areas expose dedicated enums. |
| `fsmonitor` | 58 | 0 | 0 / 0 | merge into `worktree::fsmonitor` | One path for everything else the ratchet flags | Duplicate of the worktree fsmonitor submodule; root crate has zero external imports. |
| `gitbutler` | 62 | 4 (cli 3, tests 1) | 1 / 0 | keep | — | GitButler workspace integration markers and helpers. |
| `ident` | 227 | 52 (cli 6, examples 6, http 1, ts 1, tests 38) | 0 / 0 | keep | One path for pathspec and ident | Canonical root `ident` per `canonical-modules.toml`; config ident shims merge here. |
| `index` | 653 | 122 (cli 6, utils 1, examples 8, tests 107) | 3 / 11 | keep | Visibility sweep: refs, config, worktree, index and small root modules | Index file, sparse checkout, split index, and untracked cache. |
| `line_range` | 45 | 1 (tests 1) | 0 / 0 | keep | — | Shared line-range parsing for blame and diff hunks. |
| `list_limit` | 23 | 3 (cli 3) | 0 / 0 | keep | — | Shared `-n` limit parsing for list commands and library callers. |
| `lockfile` | 146 | 11 (cli 3, examples 1, tests 7) | 0 / 1 | keep | — | Ref and index lock helpers exposed for embedding callers. |
| `maintenance` | 442 | 47 (cli 3, utils 2, examples 1, http 1, tests 40) | 20 / 3 | keep | Visibility sweep: objects, maintenance, bundle | Gc, repack, fsck, commit-graph, and MIDX maintenance; `fsck` helpers are crate-private, `repository_fsck` stays public. |
| `notes` | 136 | 14 (cli 1, utils 1, examples 1, tests 11) | 7 / 0 | keep | — | Git notes read/write on `Repository::notes`. |
| `object_inspect` | 114 | 12 (cli 7, tests 5) | 1 / 0 | merge into `revwalk::object_inspect` | One path for everything else the ratchet flags | Root re-export duplicates revwalk object display/resolve helpers. |
| `objects` | 2104 | 657 (cli 58, utils 9, examples 41, protocol 4, http 6, ts 3, tests 536) | 14 / 23 | keep | Pluggable object store on Repository | ODB, packs, MIDX, promisor, and hashing; nested barrels export consumer paths only (`odb::store` unchanged). **Done:** a custom primary store is set only through `RepositoryOptions::with_object_store`; `OdbBuilder` is path-free layering (`new`, `push_read_source`, `commit_graph`, `alternates`) and `Repository::open` resolves `objects/` itself. |
| `operation` | 210 | 21 (cli 7, tests 14) | 0 / 0 | keep | — | In-progress merge/rebase/cherry-pick operation state. |
| `pack_reuse` | 33 | 0 | 1 / 0 | merge into `objects::pack_reuse` | One path for everything else the ratchet flags | Flat root alias; canonical path is under `objects`. |
| `pack_write` | 39 | 0 | 0 / 0 | merge into `objects::pack_write` | One path for everything else the ratchet flags | Flat root alias; canonical path is under `objects`. |
| `parallel` | 129 | 36 (cli 2, ts 1, tests 33) | 0 / 0 | keep | — | Thread policy and parallel helpers used across hot paths. |
| `path_validate` | 142 | 5 (tests 5) | 0 / 0 | keep | — | Path component validation shared by index, worktree, and fsck. |
| `pathspec` | 239 | 0 | 1 / 6 | keep | One path for pathspec and ident | Canonical root pathspec matcher; overlaps with `worktree::pathspec` and porcelain helpers. |
| `porcelain` | 3142 | 327 (cli 39, utils 3, examples 7, http 1, ts 1, tests 276) | 117 / 17 | keep | Collapse function variant families | High-level command orchestration; many `Repository`-first free functions move onto handles. |
| `prelude` | 1314 | 5 (cli 1, utils 1, tests 3) | 0 / 0 | keep | — | Curated import surface for embedders. |
| `progress` | 291 | 376 (cli 29, examples 6, protocol 1, http 1, tests 339) | 0 / 7 | keep | One way to do each thing | Single progress/cancel traits; duplicate sinks merge here. |
| `quote_path` | 5 | 3 (cli 1, tests 2) | 0 / 1 | merge into `porcelain::quote_path` | One path for everything else the ratchet flags | Root re-export only; CLI uses porcelain path. |
| `refs` | 2265 | 303 (cli 16, utils 8, examples 18, protocol 3, http 2, ts 1, tests 255) | 35 / 13 | keep | Visibility sweep: refs, config, worktree, index and small root modules | Ref backends, reflog, refspec, reftable, and validation. |
| `remote` | 5450 | 214 (cli 25, utils 3, examples 7, protocol 7, http 9, tests 163) | 48 / 19 | keep | Visibility sweep: remote | Fetch/push, transport, credentials, and protocol v2. |
| `repo` | 250 | 737 (cli 56, utils 7, examples 51, protocol 3, http 4, tests 616) | 2 / 4 | keep | Collapse function variant families | `Repository` entry point; pathspec/patch-id/display helpers belong on handles. |
| `repo_handles` | 774 | 31 (cli 7, examples 3, tests 21) | 0 / 5 | keep | One way to do each thing | Bundled domain handles; naming aligns with `objects`/`references` in a later pass. |
| `revwalk` | 3181 | 180 (cli 25, utils 2, examples 14, tests 139) | 134 / 12 | keep | Visibility sweep: revwalk, diff, porcelain, commit, blame, apply | Rev-parse, rev-list, merge-base, history edits, and object inspect. |
| `stash` | 231 | 20 (cli 1, tests 19) | 3 / 0 | keep | — | Stash stack types and operations on `Repository::stash`. |
| `state` | 196 | 67 (cli 18, utils 1, examples 6, http 1, tests 41) | 5 / 0 | keep | — | Repository metadata state (MERGE_HEAD, CHERRY_PICK_HEAD, etc.). |
| `tree_sha256` | 275 | 11 (cli 1, utils 1, examples 2, tests 7) | 2 / 0 | keep | — | Tree SHA-256 verification helpers for fsck and display. |
| `worktree` | 1364 | 69 (cli 9, utils 1, examples 1, tests 58) | 27 / 14 | keep | Visibility sweep: refs, config, worktree, index and small root modules | Worktrees, ignore, attributes, and canonical `worktree::pathspec`. |
| `HookName` (re-export) | 10 | 2 (tests 2) | 0 / 0 | merge into `command_runner::hooks` | Remove root-level leftover re-exports | Hooks enum belongs in command_runner; crate root re-export is legacy. |
| `PostOperationHooks` (re-export) | 3 | 59 (cli 4, examples 1, tests 54) | 0 / 0 | merge into `command_runner::hooks` | Remove root-level leftover re-exports | Same as `HookName`. |
| `RefStorageFormat` (re-export) | 3 | 71 (examples 7, tests 64) | 0 / 0 | merge into `refs::ref_storage` | Remove root-level leftover re-exports | Duplicate of `refs::ref_storage::RefStorageFormat` at crate root. |
| `OidAbbrevBatch` (re-export) | 15 | 2 (cli 1, tests 1) | 0 / 0 | merge into `objects` (abbrev batch) | Remove root-level leftover re-exports | Object-id abbreviation helper should live under objects/revwalk, not crate root. |
| `PARALLEL_STAT_MIN_ENTRIES` | 1 | 8 (tests 8) | 0 / 0 | make private | Test-only items out of the public API | Parallel stat tuning constant for worktree scan; not embedder API. |
| `checkout_spool_probe_count` / `reset_checkout_spool_probe` | 0 | 0 | 0 / 0 | make private | Test-only items out of the public API | `#[cfg(test)]` probes; absent from `public-api.txt` but tracked for the sweep. |
| `worktree_*_probe*` (doc hidden) | 0 | 5 (tests 5) | 0 / 0 | make private | Test-only items out of the public API | Doc-hidden worktree scan probes for tests only. |
| `fuzz` (doc hidden module) | 0 | 1 (tests 1) | 0 / 0 | make private | Test-only items out of the public API | Fuzzing hooks for in-crate tests; not part of supported API. |

### Table 2 — `Repository` handle accessors

Listed accessors match [`Repository` methods](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) in `public-api.txt`. **Lines** count public-api entries mentioning the handle type. **External** is files calling `.<accessor>(` in consumer trees.

| Accessor | Handle type | Lines | External `.accessor()` files | Ratchet | Decision | Owner | Why |
| --- | --- | ---: | ---: | --- | --- | --- | --- |
| `bundles` | `Bundles` | 28 | 8 | — | **done** | One way to do each thing | Handle type renamed to match domain naming (`Bundles`). |
| `config_handle` | `ConfigHandle` | 43 | 6 | — | merge into `config()` accessor naming | One way to do each thing | Pairs with `Repository::config()` snapshot; `_handle` suffix inconsistent with `objects`/`references`. |
| `diff` | `Diff` | 59 | 26 | — | keep | — | Focused diff handle; primary entry for tree/content diffs. |
| `hooks` | `RepositoryHooks` | 18 | 6 | — | keep | — | Hook dispatch on the repository. |
| `index` | `IndexFile` | 55 | 117 | — | **done** | One way to do each thing | Accessor is `index()`; handle is `IndexFile` (distinct from `index::Index`). |
| `notes` | `RepositoryNotes` | 26 | 10 | — | keep | — | Notes mutations and reads. |
| `objects` | `Objects` | 55 | 23 | — | keep | Pluggable object store on Repository | Object read/write and ODB access. |
| `references` | `References` | 49 | 29 | — | keep | — | Ref read/write listing. |
| `reflogs` | `RepositoryReflogs` | 22 | 9 | — | keep | — | Reflog access. |
| `remotes` | `Remotes` | 34 | 19 | — | keep | Visibility sweep: remote | Remote configuration and transport entry. |
| `revwalk` | `RevWalk` | 84 | 29 | — | **done** | One way to do each thing | Accessor is `revwalk()` to match the `revwalk` module name. |
| `stash` | `RepositoryStash` | 34 | 24 | — | keep | — | Stash operations. |
| `workspace_snapshot` | `RepositoryWorkspaceSnapshot` | 21 | 0 | — | keep | — | GitButler/workspace snapshot porcelain; CLI uses free functions today. |
| `worktrees` | `Worktrees` | 24 | 9 | — | keep | Visibility sweep: refs, config, worktree, index and small root modules | Linked worktree administration. |

## Gates

These checks guard the public API on the path to **0.6.0** and afterward.

### Semver check (`make semver-check`)

[`scripts/semver-check.sh`](../scripts/semver-check.sh) runs:

```bash
cargo semver-checks check-release -p grit-lib \
  --baseline-rev "${SEMVER_BASELINE:-$(read from api-ratchet.toml semver_baseline)}" \
  --features http-ureq
```

- **Baseline (pre-0.6.0):** [`api-ratchet.toml`](api-ratchet.toml) `semver_baseline` — the commit where the supported surface was last ratified (update alongside [`public-api.txt`](public-api.txt) when you intentionally break the API). Override with `SEMVER_BASELINE=…`. After **0.6.0**, drop `semver_baseline` and use the latest `v*` tag only.
- **Surface:** same optional features as the public API snapshot (`http-ureq` only; no `test-support`).
- **Toolchain:** the workspace pin in [`rust-toolchain.toml`](../rust-toolchain.toml) (currently **1.99.0**). `cargo-semver-checks` builds baseline rustdoc in `target/semver-checks/`; that is separate from normal `cargo build` output.
- **Local run:** `make semver-check` (installs the pinned `cargo-semver-checks` on first use; OpenSSL development headers are required on Linux, same as `cargo-public-api`).
- **CI:** the **semver-check** job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) uses `fetch-depth: 0` so baseline commits and tags resolve. Until **0.6.0** the job may set `continue-on-error: true` as a safety net; the pinned `semver_baseline` should keep it green on `main`.
- **At 0.6.0:** remove `semver_baseline`, compare to the **0.6.0** tag, and drop `continue-on-error` so **0.6.x** merges require additive-only API changes (see [Stability policy](#stability-policy)).

### Public API snapshot (`make public-api-check`)

Regenerate with `make public-api` when you intentionally change the supported surface; CI fails if the committed [`public-api.txt`](public-api.txt) drifts.

### API ratchet (`make api-ratchet-check`)

Quality counts in [`api-ratchet.toml`](api-ratchet.toml) must not rise without an intentional fix; update the baseline with `make api-ratchet` when counts fall. See [`TESTING.md`](../TESTING.md).

### Results (visibility sweep: refs, config, worktree, index and small root modules)

| Metric | Before (post–objects sweep on `main`) | After (this step) |
| --- | ---: | ---: |
| `public_api_lines` | **28 373** | **27 980** (−393, ~1.4%) |
| Top-level `pub mod grit_lib::*` | **36** | **36** |

**Done in this step:** removed orphan root `bench_fixture` tree; `config::ident_config` and nested helpers crate-private where tests/CLI allow; `PARALLEL_STAT_MIN_ENTRIES` moved behind `test-support` counters; bulk `pub(crate)` on symbols not referenced from `grit-lib/tests`, `grit-cli`, or examples (scripts under `scripts/visibility_sweep_*.py`, `restore_public_from_compile_errors.py`); rustdoc links to privatized items rewritten to plain names; review follow-up kept `#[doc(hidden)] pub mod repo_relpath` and reverted accidental `pub` expansions; [`api-ratchet.toml`](api-ratchet.toml) baseline updated to **27 980**.

**Ratchet:** Plan target ≤ 29 300 lines is met (**27 980**). Cumulative reduction from the original audit baseline (32 590) is **4 610** lines (~14.1%).

**Top-level module count:** Still **36** (target ≤ 33 not met). `repo_relpath` stays `#[doc(hidden)] pub mod` (CLI path display); `ident` and `worktree::pathspec` unchanged per canonical-modules guidance.

**Audit rows (this step):** `bench_fixture` root alias — **done**. `PARALLEL_STAT_MIN_ENTRIES`, probe hooks — **done** or owned by test-only item. Domain modules — **keep** with nested surface reduced where tests/CLI allow; branch-tracking / pathspec types used by CLI remain **public** (merge into repository handles deferred to later API items).

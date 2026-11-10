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
- Public modules re-export implementation items by name (`pub use crate::x::{A, B};`), never with a glob (`pub use crate::x::*;`), so a new `pub` item in an implementation file does not silently become public API. Items only other crate modules need are re-exported with `pub(crate) use`.
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
| Merge family | `repo.merge()` | `merge::Merge` | Merge, pick, revert, replay, `--check` predictions, operation state (`--continue`, `--abort`, `--skip`), rerere |
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

<!-- api-audit:begin -->
Snapshot: [`api-ratchet.toml`](api-ratchet.toml) `meta.updated` **2026-11-10**; [`public-api.txt`](public-api.txt) **31,732** non-empty lines (`wc -l` on the file: **31,732**).

Regenerate this block with `make api-audit`; CI runs `make api-audit-check` in the **public-api** job next to `make api-ratchet-check`.

### Summary — ratchet counts

| Metric | Value |
| --- | ---: |
| `pub_repo_constructors` | 3 |
| `pub_repo_constructor_offenders` | 4 |
| `public_api_lines` | 31,732 |
| `pub_fn_impl_suffix` | 0 |
| `pub_fn_with_in_name` | 28 |
| `pub_fn_too_many_arguments_allow` | 2 |
| `pub_fn_two_plus_bare_bool` | 9 |
| `pub_fn_result_string_err` | 0 |
| `pub_free_fn_repository_first` | 29 |
| `pub_free_fn_git_dir_path_first` | 24 |
| `pub_free_fn_config_write` | 6 |
| `pub_items_multiple_paths` | 141 |
| `pub_libc_types` | 0 |
| `pub_unsafe_fn` | 0 |
| `pub_cancel_types` | 87 |
| `pub_progress_interfaces` | 4 |
| `pub_refname_validators` | 0 |
| `pub_fn_refname_str` | 9 |
| `pub_fn_pathspec_strings` | 0 |
| `pub_fn_configset_with_repo` | 0 |
| `umbrella_error_variants` | 20 |
| `pub_error_catch_all_variants` | 0 |
| `pub_error_string_tuple_variants` | 0 |
| `pub_error_enum_not_non_exhaustive` | 0 |
| `pub_area_error_wraps_umbrella` | 0 |
| `error_message_construction_sites` | 0 |
| `doc_example_free_fn_calls` | 0 |
| `pub_deprecated_items` | 0 |

### Top-level public modules

Top-level modules match `grep -E '^pub mod grit_lib::[a-z_0-9]+$' grit-lib/public-api.txt`. **External files** counts files under consumer trees that reference `grit_lib::<module>` (`rg -l` or `grep -Erl` on consumer trees). **Offenders** is the consolidation-total from `make api-ratchet-report` metrics in that module; **repo-first** / **`_with_`** are approximated from `public-api.txt` in the module span.

| Module | Lines | External files | Offenders | repo-first | `_with_` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `apply` | 192 | 13 | 2 | 2 | 0 |
| `blame` | 283 | 28 | 2 | 2 | 0 |
| `bundle` | 324 | 18 | 0 | 0 | 0 |
| `change_id` | 120 | 25 | 2 | 2 | 0 |
| `command_runner` | 472 | 90 | 1 | 0 | 1 |
| `commit` | 285 | 79 | 3 | 0 | 2 |
| `commits` | 272 | 61 | 0 | 0 | 0 |
| `config` | 1,000 | 780 | 0 | 0 | 0 |
| `diagnostics` | 131 | 17 | 0 | 0 | 0 |
| `diff` | 2,335 | 184 | 26 | 5 | 4 |
| `divergence` | 243 | 5 | 0 | 0 | 0 |
| `error` | 1,487 | 385 | 0 | 0 | 0 |
| `gitbutler` | 62 | 4 | 0 | 1 | 0 |
| `ident` | 190 | 63 | 0 | 0 | 0 |
| `index` | 958 | 183 | 1 | 1 | 0 |
| `line_range` | 45 | 1 | 0 | 0 | 0 |
| `list_limit` | 23 | 3 | 0 | 0 | 0 |
| `lockfile` | 127 | 13 | 1 | 0 | 1 |
| `maintenance` | 1,169 | 58 | 0 | 0 | 0 |
| `merge` | 1,116 | 61 | 0 | 3 | 1 |
| `notes` | 129 | 15 | 0 | 0 | 0 |
| `objects` | 2,371 | 716 | 2 | 2 | 1 |
| `operation` | 215 | 27 | 0 | 0 | 0 |
| `ownership_check` | 79 | 4 | 0 | 0 | 0 |
| `parallel` | 129 | 33 | 1 | 0 | 0 |
| `path_validate` | 142 | 7 | 0 | 0 | 0 |
| `porcelain` | 2,755 | 301 | 5 | 2 | 2 |
| `prelude` | 1,174 | 9 | 0 | 0 | 0 |
| `progress` | 233 | 525 | 0 | 0 | 3 |
| `refs` | 2,181 | 253 | 8 | 5 | 3 |
| `remote` | 4,947 | 227 | 1 | 3 | 1 |
| `repo` | 449 | 854 | 11 | 2 | 2 |
| `repo_handles` | 863 | 54 | 4 | 0 | 4 |
| `revwalk` | 2,945 | 210 | 2 | 2 | 2 |
| `stash` | 335 | 28 | 3 | 3 | 0 |
| `state` | 142 | 52 | 0 | 0 | 0 |
| `status` | 340 | 5 | 1 | 0 | 1 |
| `tree_sha256` | 276 | 11 | 2 | 2 | 0 |
| `worktree` | 1,192 | 125 | 13 | 10 | 4 |

### `Repository` handle accessors

Listed accessors are `Repository::<name>(&self) -> …` entries in `public-api.txt` whose return type is a handle (not `Path`, `str`, or a scalar). **Lines** counts public-api entries mentioning the shortened return type. **External** counts consumer files calling `.<accessor>(` (`rg -l` or `grep -Erl` on consumer trees).

| Accessor | Return type | Lines | External `.accessor()` files |
| --- | --- | ---: | ---: |
| `branches` | `grit_lib::repo_handles::Branches<'_>` | 32 | 28 |
| `bundles` | `grit_lib::repo_handles::RepositoryBundles<'_>` | 29 | 16 |
| `checkout` | `grit_lib::repo_handles::Checkout<'_>` | 32 | 97 |
| `commits` | `grit_lib::commits::Commits<'_>` | 25 | 133 |
| `config` | `grit_lib::config::Config<'_>` | 553 | 98 |
| `diagnostics` | `grit_lib::diagnostics::DiagnosticsHandle` | 18 | 1 |
| `diff` | `grit_lib::diff::Diff<'_>` | 509 | 92 |
| `history` | `grit_lib::revwalk::history::History<'_>` | 213 | 40 |
| `hooks` | `grit_lib::repo_handles::RepositoryHooks<'_>` | 28 | 10 |
| `index` | `grit_lib::index::IndexFile<'_>` | 41 | 199 |
| `maintenance` | `grit_lib::maintenance::Maintenance<'_>` | 34 | 49 |
| `merge` | `grit_lib::merge::Merge<'_>` | 142 | 33 |
| `notes` | `grit_lib::repo_handles::RepositoryNotes<'_>` | 33 | 14 |
| `objects` | `grit_lib::objects::Objects<'_>` | 29 | 30 |
| `operation` | `grit_lib::operation::Operations<'_>` | 23 | 12 |
| `references` | `grit_lib::refs::References<'_>` | 36 | 234 |
| `reflogs` | `grit_lib::repo_handles::RepositoryReflogs<'_>` | 24 | 21 |
| `remotes` | `grit_lib::repo_handles::RepositoryRemotes<'_>` | 44 | 89 |
| `revwalk` | `grit_lib::revwalk::handle::RevWalk<'_>` | 164 | 157 |
| `snapshot_checkouts` | `grit_lib::repo_handles::RepositorySnapshotCheckouts<'_>` | 39 | 4 |
| `stash` | `grit_lib::repo_handles::RepositoryStash<'_>` | 34 | 28 |
| `status` | `grit_lib::status::Status<'_>` | 101 | 343 |
| `tags` | `grit_lib::repo_handles::Tags<'_>` | 23 | 18 |
| `workspace_snapshot` | `grit_lib::repo_handles::RepositoryWorkspaceSnapshot<'_>` | 21 | 0 |
| `worktrees` | `grit_lib::repo_handles::RepositoryWorktrees<'_>` | 31 | 26 |

### Residuals frozen in 0.6

Remaining ratchet offenders at the snapshot above. These counts may only go down until **0.6.0** freezes the supported surface; see [`api-ratchet.toml`](api-ratchet.toml).

| Metric | Offender |
| --- | --- |
| `pub_cancel_types` | `add_with_cancel (grit-lib/src/repo_handles/worktree.rs:90) :: add_with_cancel(cancel)` |
| `pub_cancel_types` | `apply_reference (grit-lib/src/repo_handles/stash.rs:155) :: apply_reference(cancel)` |
| `pub_cancel_types` | `blame (grit-lib/src/diff/handle.rs:792) :: blame(cancel)` |
| `pub_cancel_types` | `checkout (grit-lib/src/repo_handles/checkout.rs:76) :: checkout(cancel)` |
| `pub_cancel_types` | `checkout_hard_to_tree (grit-lib/src/repo_handles/checkout.rs:87) :: checkout_hard_to_tree(cancel)` |
| `pub_cancel_types` | `continue_merge (grit-lib/src/merge/handle.rs:182) :: continue_merge(cancel)` |
| `pub_cancel_types` | `continue_sequence (grit-lib/src/merge/handle.rs:239) :: continue_sequence(cancel)` |
| `pub_cancel_types` | `create_staged (grit-lib/src/commits/handle.rs:134) :: create_staged(cancel)` |
| `pub_cancel_types` | `grit_lib::apply::index_apply::apply_patch_to_index_and_write :: apply_patch_to_index_and_write(cancel)` |
| `pub_cancel_types` | `grit_lib::bundle::core::Bundle::unbundle :: unbundle(cancel)` |
| `pub_cancel_types` | `grit_lib::clone::clone :: clone(cancel)` |
| `pub_cancel_types` | `grit_lib::clone::clone_into :: clone_into(cancel)` |
| `pub_cancel_types` | `grit_lib::commits::handle::Commits::create :: create(cancel)` |
| `pub_cancel_types` | `grit_lib::commits::handle::Commits::plan :: plan(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::apply :: apply(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::apply_absorb :: apply_absorb(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::apply_fixup :: apply_fixup(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::apply_fixup_hunks :: apply_fixup_hunks(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::continue_rebase :: continue_rebase(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::skip_rebase :: skip_rebase(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::start_rebase :: start_rebase(cancel)` |
| `pub_cancel_types` | `grit_lib::history_root::handle::History::undo_last :: undo_last(cancel)` |
| `pub_cancel_types` | `grit_lib::index::handle::IndexFile::stage :: stage(cancel)` |
| `pub_cancel_types` | `grit_lib::index_pack::ingest_received_pack_path :: ingest_received_pack_path(cancel)` |
| `pub_cancel_types` | `grit_lib::index_pack::install_pack_path :: install_pack_path(cancel)` |
| `pub_cancel_types` | `grit_lib::maintenance::handle::Maintenance::fsck :: fsck(cancel)` |
| `pub_cancel_types` | `grit_lib::maintenance::handle::Maintenance::gc :: gc(cancel)` |
| `pub_cancel_types` | `grit_lib::maintenance::handle::Maintenance::gc_auto :: gc_auto(cancel)` |
| `pub_cancel_types` | `grit_lib::maintenance::handle::Maintenance::repack :: repack(cancel)` |
| `pub_cancel_types` | `grit_lib::maintenance::handle::Maintenance::repack_all :: repack_all(cancel)` |
| `pub_cancel_types` | `grit_lib::merge::handle::Merge::integrate_into_branch :: integrate_into_branch(cancel)` |
| `pub_cancel_types` | `grit_lib::porcelain::pull::pull :: pull(cancel)` |
| `pub_cancel_types` | `grit_lib::progress::CancelToken :: CancelToken` |
| `pub_cancel_types` | `grit_lib::progress::poll_cancel :: poll_cancel(cancel)` |
| `pub_cancel_types` | `grit_lib::remote::core::Remote::fetch :: fetch(cancel)` |
| `pub_cancel_types` | `grit_lib::remote::core::Remote::push :: push(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::bundle::RepositoryBundles::fetch :: fetch(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::bundle::RepositoryBundles::write :: write(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::bundle::RepositoryBundles::write_from_revs :: write_from_revs(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::bundle::RepositoryBundles::write_to :: write_to(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::checkout::Checkout::clean :: clean(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::remote::RepositoryRemotes::fetch :: fetch(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::remote::RepositoryRemotes::fetch_all :: fetch_all(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::remote::RepositoryRemotes::fetch_from :: fetch_from(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::remote::RepositoryRemotes::push :: push(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::remote::RepositoryRemotes::push_to :: push_to(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::stash::RepositoryStash::apply :: apply(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::stash::RepositoryStash::apply_commit :: apply_commit(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::stash::RepositoryStash::pop :: pop(cancel)` |
| `pub_cancel_types` | `grit_lib::repo_handles::stash::RepositoryStash::push :: push(cancel)` |
| `pub_cancel_types` | `grit_lib::resumable::download_resumable :: download_resumable(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::commits :: commits(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::commits_iter :: commits_iter(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::commits_stream :: commits_stream(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::count :: count(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::describe :: describe(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::left_right_counts :: left_right_counts(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::rev_list :: rev_list(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::rev_list_result :: rev_list_result(cancel)` |
| `pub_cancel_types` | `grit_lib::revwalk::handle::RevWalk::walk :: walk(cancel)` |
| `pub_cancel_types` | `grit_lib::serve::receive_pack::ReceivePack::with_cancel :: with_cancel(cancel)` |
| `pub_cancel_types` | `grit_lib::serve::upload_pack::UploadPack::with_cancel :: with_cancel(cancel)` |
| `pub_cancel_types` | `grit_lib::snapshot_checkout::handle::RepositorySnapshotCheckouts::fetch_branch :: fetch_branch(cancel)` |
| `pub_cancel_types` | `grit_lib::snapshot_checkout::handle::RepositorySnapshotCheckouts::push_for_checkin :: push_for_checkin(cancel)` |
| `pub_cancel_types` | `grit_lib::snapshot_checkout::handle::RepositorySnapshotCheckouts::refresh :: refresh(cancel)` |
| `pub_cancel_types` | `grit_lib::snapshot_checkout::materialize :: materialize(cancel)` |
| `pub_cancel_types` | `grit_lib::status::handle::Status::run :: run(cancel)` |
| `pub_cancel_types` | `grit_lib::status::handle::Status::untracked :: untracked(cancel)` |
| `pub_cancel_types` | `grit_lib::transport::ConnectOptions :: field cancel` |
| `pub_cancel_types` | `integrate (grit-lib/src/merge/handle.rs:133) :: integrate(cancel)` |
| `pub_cancel_types` | `move_path (grit-lib/src/repo_handles/checkout.rs:147) :: move_path(cancel)` |
| `pub_cancel_types` | `move_paths (grit-lib/src/repo_handles/checkout.rs:158) :: move_paths(cancel)` |
| `pub_cancel_types` | `pick (grit-lib/src/merge/handle.rs:197) :: pick(cancel)` |
| `pub_cancel_types` | `pop_reference (grit-lib/src/repo_handles/stash.rs:166) :: pop_reference(cancel)` |
| `pub_cancel_types` | `remove_paths (grit-lib/src/repo_handles/checkout.rs:138) :: remove_paths(cancel)` |
| `pub_cancel_types` | `reset (grit-lib/src/repo_handles/checkout.rs:108) :: reset(cancel)` |
| `pub_cancel_types` | `restore (grit-lib/src/repo_handles/checkout.rs:118) :: restore(cancel)` |
| `pub_cancel_types` | `restore (grit-lib/src/repo_handles/workspace_snapshot.rs:42) :: restore(cancel)` |
| `pub_cancel_types` | `revert_commit (grit-lib/src/merge/handle.rs:208) :: revert_commit(cancel)` |
| `pub_cancel_types` | `skip_sequence_step (grit-lib/src/merge/handle.rs:249) :: skip_sequence_step(cancel)` |
| `pub_cancel_types` | `stage_for_commit (grit-lib/src/index/handle.rs:250) :: stage_for_commit(cancel)` |
| `pub_cancel_types` | `stage_hunks (grit-lib/src/index/handle.rs:338) :: stage_hunks(cancel)` |
| `pub_cancel_types` | `stage_on (grit-lib/src/index/handle.rs:263) :: stage_on(cancel)` |
| `pub_cancel_types` | `start_sequence (grit-lib/src/merge/handle.rs:229) :: start_sequence(cancel)` |
| `pub_cancel_types` | `switch (grit-lib/src/repo_handles/checkout.rs:66) :: switch(cancel)` |
| `pub_cancel_types` | `unbundle (grit-lib/src/repo_handles/bundle.rs:224) :: unbundle(cancel)` |
| `pub_cancel_types` | `untracked_with_meta (grit-lib/src/status/handle.rs:205) :: untracked_with_meta(cancel)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::branches::Branches::delete :: delete(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::add :: add(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::get :: get(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::inspect :: inspect(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::remove :: remove(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::set_head :: set_head(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::RepositoryRemotes::set_url :: set_url(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::remote::has_remote_section :: has_remote_section(name)` |
| `pub_fn_refname_str` | `grit_lib::repo_handles::stash::RepositoryStash::branch :: branch(name)` |
| `pub_fn_too_many_arguments_allow` | `grit_lib::blame::compute_blame :: compute_blame` |
| `pub_fn_too_many_arguments_allow` | `grit_lib::merge_diff::convert_blob_to_worktree_for_path :: convert_blob_to_worktree_for_path` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::blame::compute_blame :: compute_blame` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::combined_tree_diff::format_combined_raw_line_all_paths :: format_combined_raw_line_all_paths` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::commit_trailers::finalize_cherry_pick_message :: finalize_cherry_pick_message` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::diff::options::DiffOptions::resolve_indent_heuristic :: resolve_indent_heuristic` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::ignore::IgnorePatternList (grit-lib/src/ignore.rs:166) :: is_ignored` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::ignore::IgnorePatternList::last_match :: last_match` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::index::handle::IndexFile (grit-lib/src/index/handle.rs:363) :: restore_hunks` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::odb::store::files::FilesSource::open :: open` |
| `pub_fn_two_plus_bare_bool` | `grit_lib::porcelain::paths::RemoveBlockReason (grit-lib/src/porcelain/paths.rs:98) :: classify` |
| `pub_fn_with_in_name` | `grit_lib::combined_tree_diff::format_combined_raw_line_all_paths_with_options :: format_combined_raw_line_all_paths_with_options` |
| `pub_fn_with_in_name` | `grit_lib::command_runner::RunningCommand::wait_with_output :: wait_with_output` |
| `pub_fn_with_in_name` | `grit_lib::commit_trailers::append_signoff_trailer_with_dedup :: append_signoff_trailer_with_dedup` |
| `pub_fn_with_in_name` | `grit_lib::commit_trailers::finalize_cherry_pick_message_with_options :: finalize_cherry_pick_message_with_options` |
| `pub_fn_with_in_name` | `grit_lib::crlf::ConversionConfig (grit-lib/src/crlf.rs:321) :: from_config_with_runner` |
| `pub_fn_with_in_name` | `grit_lib::diff::handle::Diff (grit-lib/src/diff/handle.rs:814) :: resolve_blame_line_range_with_options` |
| `pub_fn_with_in_name` | `grit_lib::diff::options::DiffOptions (grit-lib/src/diff/options.rs:271) :: resolve_indent_heuristic_with_override` |
| `pub_fn_with_in_name` | `grit_lib::gc::prune_loose_unreachable_with_dry_run :: prune_loose_unreachable_with_dry_run` |
| `pub_fn_with_in_name` | `grit_lib::lockfile::LockFile (grit-lib/src/lockfile.rs:244) :: acquire_with_clock` |
| `pub_fn_with_in_name` | `grit_lib::log_walk::LogWalkCursor (grit-lib/src/log_walk.rs:290) :: decode_with_git_dir` |
| `pub_fn_with_in_name` | `grit_lib::merge_diff::blob_text_for_diff_with_oid :: blob_text_for_diff_with_oid` |
| `pub_fn_with_in_name` | `grit_lib::odb::store::files::FilesSource (grit-lib/src/odb/store/files.rs:96) :: open_with_options` |
| `pub_fn_with_in_name` | `grit_lib::parallel::map::try_par_map_with_local :: try_par_map_with_local` |
| `pub_fn_with_in_name` | `grit_lib::pathspec::bloom_lookup_prefix_with_cwd :: bloom_lookup_prefix_with_cwd` |
| `pub_fn_with_in_name` | `grit_lib::pathspec::resolve_pathspec_with_argv_precompose :: resolve_pathspec_with_argv_precompose` |
| `pub_fn_with_in_name` | `grit_lib::porcelain::paths::RemoveBlockReason (grit-lib/src/porcelain/paths.rs:109) :: allowed_with_cached` |
| `pub_fn_with_in_name` | `grit_lib::quote_path_root::format_diff_path_with_prefix :: format_diff_path_with_prefix` |
| `pub_fn_with_in_name` | `grit_lib::refs::prepare_with_lock_wait :: prepare_with_lock_wait` |
| `pub_fn_with_in_name` | `grit_lib::refs::store::validation::verify_create_casefold_with_lookup :: verify_create_casefold_with_lookup` |
| `pub_fn_with_in_name` | `grit_lib::repo::InitOptions (grit-lib/src/repo.rs:291) :: with_bare_with_worktree_at_path` |
| `pub_fn_with_in_name` | `grit_lib::repo_caches::RepoCaches::load_config_with_options :: load_config_with_options` |
| `pub_fn_with_in_name` | `grit_lib::repo_handles::branches::Branches (grit-lib/src/repo_handles/branches.rs:130) :: compare_with_target` |
| `pub_fn_with_in_name` | `grit_lib::repo_handles::notes::RepositoryNotes::list_with_bodies :: list_with_bodies` |
| `pub_fn_with_in_name` | `grit_lib::repo_handles::worktree::RepositoryWorktrees (grit-lib/src/repo_handles/worktree.rs:105) :: find_with_branch_checked_out` |
| `pub_fn_with_in_name` | `grit_lib::repo_handles::worktree::RepositoryWorktrees (grit-lib/src/repo_handles/worktree.rs:90) :: add_with_cancel` |
| `pub_fn_with_in_name` | `grit_lib::rev_list::RevListResult (grit-lib/src/rev_list.rs:870) :: count_with_objects_total` |
| `pub_fn_with_in_name` | `grit_lib::status::handle::Status (grit-lib/src/status/handle.rs:205) :: untracked_with_meta` |
| `pub_fn_with_in_name` | `grit_lib::worktree_cwd::cwd_would_be_removed_with_dir :: cwd_would_be_removed_with_dir` |
| `pub_free_fn_config_write` | `add_value (grit-lib/src/config/write.rs:154) :: add_value` |
| `pub_free_fn_config_write` | `remove_section (grit-lib/src/config/write.rs:228) :: remove_section` |
| `pub_free_fn_config_write` | `rename_section (grit-lib/src/config/write.rs:211) :: rename_section` |
| `pub_free_fn_config_write` | `set (grit-lib/src/config/write.rs:129) :: set` |
| `pub_free_fn_config_write` | `unset (grit-lib/src/config/write.rs:168) :: unset` |
| `pub_free_fn_config_write` | `unset_all (grit-lib/src/config/write.rs:190) :: unset_all` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::gc::remote_default_branch_local :: remote_default_branch_local` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::gc::update_refs :: update_refs` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::blob_text_for_diff :: blob_text_for_diff` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::convert_blob_to_worktree_for_path :: convert_blob_to_worktree_for_path` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_attr_external_driver :: diff_attr_external_driver` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_attr_forces_binary :: diff_attr_forces_binary` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_attr_forces_text :: diff_attr_forces_text` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_driver_forces_text :: diff_driver_forces_text` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_forced_binary_by_driver :: diff_forced_binary_by_driver` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::diff_textconv_active :: diff_textconv_active` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::format_combined_textconv_patch :: format_combined_textconv_patch` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::format_worktree_conflict_combined :: format_worktree_conflict_combined` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::merge_diff::is_binary_for_diff :: is_binary_for_diff` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::porcelain::merge::squash_conflict_merge_in_progress :: squash_conflict_merge_in_progress` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::common_git_dir_for_config :: common_git_dir_for_config` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::early_config_ignore_repo_reason :: early_config_ignore_repo_reason` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::ensure_core_bare :: ensure_core_bare` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::init_worktree_config :: init_worktree_config` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::local_config_write_path :: local_config_write_path` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::repository_format_warning :: repository_format_warning` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::resolve_git_directory_arg :: resolve_git_directory_arg` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::validate_repo_format :: validate_repo_format` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::repo::worktree_config_enabled :: worktree_config_enabled` |
| `pub_free_fn_git_dir_path_first` | `grit_lib::shallow::load_shallow_oids :: load_shallow_oids` |
| `pub_free_fn_repository_first` | `grit_lib::apply::index_apply::apply_patch_to_index :: apply_patch_to_index` |
| `pub_free_fn_repository_first` | `grit_lib::apply::index_apply::apply_patch_to_index_and_write :: apply_patch_to_index_and_write` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::attribute_matching_ignore_case :: attribute_matching_ignore_case` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::load_gitattributes_for_check_attr_cached :: load_gitattributes_for_check_attr_cached` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::load_gitattributes_for_check_attr_source :: load_gitattributes_for_check_attr_source` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::load_gitattributes_stack :: load_gitattributes_stack` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::path_relative_to_worktree :: path_relative_to_worktree` |
| `pub_free_fn_repository_first` | `grit_lib::attributes::resolve_tree_oid :: resolve_tree_oid` |
| `pub_free_fn_repository_first` | `grit_lib::change_id::find_commits_by_change_id_prefix :: find_commits_by_change_id_prefix` |
| `pub_free_fn_repository_first` | `grit_lib::change_id::resolve_commit_by_change_id_prefix :: resolve_commit_by_change_id_prefix` |
| `pub_free_fn_repository_first` | `grit_lib::diff::patch_log::write_commit_log_patch :: write_commit_log_patch` |
| `pub_free_fn_repository_first` | `grit_lib::diff::patch_log::write_commits_log_patch :: write_commits_log_patch` |
| `pub_free_fn_repository_first` | `grit_lib::diff::patch_log::write_patch :: write_patch` |
| `pub_free_fn_repository_first` | `grit_lib::diff::patch_log::write_patch_resolved :: write_patch_resolved` |
| `pub_free_fn_repository_first` | `grit_lib::diff::patch_log::write_patch_structured :: write_patch_structured` |
| `pub_free_fn_repository_first` | `grit_lib::ignore::normalize_repo_relative :: normalize_repo_relative` |
| `pub_free_fn_repository_first` | `grit_lib::merge_trees::apply_tree_merge_output_to_worktree_index :: apply_tree_merge_output_to_worktree_index` |
| `pub_free_fn_repository_first` | `grit_lib::merge_trees::merge_trees_three_way :: merge_trees_three_way` |
| `pub_free_fn_repository_first` | `grit_lib::merge_trees::refresh_tree_merge_index_stat_cache :: refresh_tree_merge_index_stat_cache` |
| `pub_free_fn_repository_first` | `grit_lib::porcelain::pull::pull :: pull` |
| `pub_free_fn_repository_first` | `grit_lib::porcelain::stash::check_stash_apply_would_overwrite_local_changes :: check_stash_apply_would_overwrite_local_changes` |
| `pub_free_fn_repository_first` | `grit_lib::porcelain::stash::parse_stash_reference :: parse_stash_reference` |
| `pub_free_fn_repository_first` | `grit_lib::porcelain::stash::stash_index_for_oid :: stash_index_for_oid` |
| `pub_free_fn_repository_first` | `grit_lib::refs::listing::ref_query_options_for_repo :: ref_query_options_for_repo` |
| `pub_free_fn_repository_first` | `grit_lib::refs::query_reachability::resolve_commit_filter_target :: resolve_commit_filter_target` |
| `pub_free_fn_repository_first` | `grit_lib::refs_fsck::refs_fsck :: refs_fsck` |
| `pub_free_fn_repository_first` | `grit_lib::status::case_collision::worktree_case_collision_groups :: worktree_case_collision_groups` |
| `pub_free_fn_repository_first` | `grit_lib::tree_sha256::tag_verify::verify_tag :: verify_tag` |
| `pub_free_fn_repository_first` | `grit_lib::tree_sha256::tree_sha256 :: tree_sha256` |
| `pub_items_multiple_paths` | `id 1066: grit_lib::diff::rerere::RerereEvent | grit_lib::merge::RerereEvent` |
| `pub_items_multiple_paths` | `id 1067: grit_lib::commits::CommitOutcome | grit_lib::porcelain::commit::CommitOutcome` |
| `pub_items_multiple_paths` | `id 10996: grit_lib::objects::hash::Parallelism | grit_lib::parallel::Parallelism` |
| `pub_items_multiple_paths` | `id 1133: grit_lib::commits::CommitError | grit_lib::porcelain::CommitError` |
| `pub_items_multiple_paths` | `id 1207: grit_lib::merge::MergeError | grit_lib::porcelain::MergeError` |
| `pub_items_multiple_paths` | `id 14360: grit_lib::error::PathspecResolveError | grit_lib::worktree::pathspec::PathspecResolveError` |
| `pub_items_multiple_paths` | `id 14875: grit_lib::diff::merge_diff::all_blob_paths_in_tree_order | grit_lib::merge::merge_diff::all_blob_paths_in_tree_order` |
| `pub_items_multiple_paths` | `id 14877: grit_lib::diff::merge_diff::blob_oid_at_path | grit_lib::merge::merge_diff::blob_oid_at_path` |
| `pub_items_multiple_paths` | `id 14879: grit_lib::diff::merge_diff::blob_text_for_diff | grit_lib::merge::merge_diff::blob_text_for_diff` |
| `pub_items_multiple_paths` | `id 14881: grit_lib::diff::merge_diff::blob_text_for_diff_with_oid | grit_lib::merge::merge_diff::blob_text_for_diff_with_oid` |
| `pub_items_multiple_paths` | `id 14883: grit_lib::diff::merge_diff::combined_diff_paths | grit_lib::merge::merge_diff::combined_diff_paths` |
| `pub_items_multiple_paths` | `id 14885: grit_lib::diff::merge_diff::combined_merge_parent_blob_paths | grit_lib::merge::merge_diff::combined_merge_parent_blob_paths` |
| `pub_items_multiple_paths` | `id 14887: grit_lib::diff::merge_diff::convert_blob_to_worktree_for_path | grit_lib::merge::merge_diff::convert_blob_to_worktree_for_path` |
| `pub_items_multiple_paths` | `id 14889: grit_lib::diff::merge_diff::diff_attr_external_driver | grit_lib::merge::merge_diff::diff_attr_external_driver` |
| `pub_items_multiple_paths` | `id 14891: grit_lib::diff::merge_diff::diff_attr_forces_binary | grit_lib::merge::merge_diff::diff_attr_forces_binary` |
| `pub_items_multiple_paths` | `id 14893: grit_lib::diff::merge_diff::diff_attr_forces_text | grit_lib::merge::merge_diff::diff_attr_forces_text` |
| `pub_items_multiple_paths` | `id 14895: grit_lib::diff::merge_diff::diff_cachetextconv_enabled | grit_lib::merge::merge_diff::diff_cachetextconv_enabled` |
| `pub_items_multiple_paths` | `id 14897: grit_lib::diff::merge_diff::diff_driver_forces_text | grit_lib::merge::merge_diff::diff_driver_forces_text` |
| `pub_items_multiple_paths` | `id 14899: grit_lib::diff::merge_diff::diff_forced_binary_by_driver | grit_lib::merge::merge_diff::diff_forced_binary_by_driver` |
| `pub_items_multiple_paths` | `id 14901: grit_lib::diff::merge_diff::diff_textconv_active | grit_lib::merge::merge_diff::diff_textconv_active` |
| `pub_items_multiple_paths` | `id 14903: grit_lib::diff::merge_diff::diff_textconv_cmd_line | grit_lib::merge::merge_diff::diff_textconv_cmd_line` |
| `pub_items_multiple_paths` | `id 14905: grit_lib::diff::merge_diff::enrich_combined_path_renames | grit_lib::merge::merge_diff::enrich_combined_path_renames` |
| `pub_items_multiple_paths` | `id 14907: grit_lib::diff::merge_diff::format_combined_binary | grit_lib::merge::merge_diff::format_combined_binary` |
| `pub_items_multiple_paths` | `id 14909: grit_lib::diff::merge_diff::format_combined_binary_header | grit_lib::merge::merge_diff::format_combined_binary_header` |
| `pub_items_multiple_paths` | `id 14911: grit_lib::diff::merge_diff::format_combined_binary_header_n | grit_lib::merge::merge_diff::format_combined_binary_header_n` |
| `pub_items_multiple_paths` | `id 14913: grit_lib::diff::merge_diff::format_combined_textconv_patch | grit_lib::merge::merge_diff::format_combined_textconv_patch` |
| `pub_items_multiple_paths` | `id 14915: grit_lib::diff::merge_diff::format_worktree_conflict_combined | grit_lib::merge::merge_diff::format_worktree_conflict_combined` |
| `pub_items_multiple_paths` | `id 14917: grit_lib::diff::merge_diff::is_binary_for_diff | grit_lib::merge::merge_diff::is_binary_for_diff` |
| `pub_items_multiple_paths` | `id 14919: grit_lib::diff::merge_diff::read_blob_at_path | grit_lib::merge::merge_diff::read_blob_at_path` |
| `pub_items_multiple_paths` | `id 14921: grit_lib::diff::merge_diff::run_textconv | grit_lib::merge::merge_diff::run_textconv` |
| `pub_items_multiple_paths` | `id 14923: grit_lib::diff::merge_diff::run_textconv_raw | grit_lib::merge::merge_diff::run_textconv_raw` |
| `pub_items_multiple_paths` | `id 14925: grit_lib::diff::merge_diff::CombinedDiffFormat | grit_lib::merge::merge_diff::CombinedDiffFormat` |
| `pub_items_multiple_paths` | `id 14927: grit_lib::diff::merge_diff::CombinedPatchOptions | grit_lib::merge::merge_diff::CombinedPatchOptions` |
| `pub_items_multiple_paths` | `id 14929: grit_lib::diff::merge_diff::CombinedPathLabels | grit_lib::merge::merge_diff::CombinedPathLabels` |
| `pub_items_multiple_paths` | `id 14931: grit_lib::diff::merge_diff::ConflictStages | grit_lib::merge::merge_diff::ConflictStages` |
| `pub_items_multiple_paths` | `id 14933: grit_lib::diff::merge_diff::Textconv | grit_lib::merge::merge_diff::Textconv` |
| `pub_items_multiple_paths` | `id 14937: grit_lib::diff::merge_file::is_binary | grit_lib::merge::merge_file::is_binary` |
| `pub_items_multiple_paths` | `id 14939: grit_lib::diff::merge_file::merge | grit_lib::merge::merge_file::merge` |
| `pub_items_multiple_paths` | `id 14943: grit_lib::diff::merge_file::MergeInput | grit_lib::merge::merge_file::MergeInput` |
| `pub_items_multiple_paths` | `id 14945: grit_lib::diff::merge_file::MergeOutput | grit_lib::merge::merge_file::MergeOutput` |
| `pub_items_multiple_paths` | `id 14948: grit_lib::diff::merge_trees::apply_tree_merge_output_to_index | grit_lib::merge::merge_trees::apply_tree_merge_output_to_index` |
| `pub_items_multiple_paths` | `id 14950: grit_lib::diff::merge_trees::apply_tree_merge_output_to_worktree_index | grit_lib::merge::merge_trees::apply_tree_merge_output_to_worktree_index` |
| `pub_items_multiple_paths` | `id 14952: grit_lib::diff::merge_trees::index_tree_oid_matches_head | grit_lib::merge::merge_trees::index_tree_oid_matches_head` |
| `pub_items_multiple_paths` | `id 14954: grit_lib::diff::merge_trees::materialized_merge_result_tree | grit_lib::merge::merge_trees::materialized_merge_result_tree` |
| `pub_items_multiple_paths` | `id 14956: grit_lib::diff::merge_trees::merge_trees_three_way | grit_lib::merge::merge_trees::merge_trees_three_way` |
| `pub_items_multiple_paths` | `id 14958: grit_lib::diff::merge_trees::refresh_tree_merge_index_stat_cache | grit_lib::merge::merge_trees::refresh_tree_merge_index_stat_cache` |
| `pub_items_multiple_paths` | `id 14960: grit_lib::diff::merge_trees::DirectoryRenameConflict | grit_lib::merge::merge_trees::DirectoryRenameConflict` |
| `pub_items_multiple_paths` | `id 14963: grit_lib::diff::merge_trees::MergeSide | grit_lib::merge::merge_trees::MergeSide` |
| `pub_items_multiple_paths` | `id 14965: grit_lib::diff::merge_trees::RenameRenameConflict | grit_lib::merge::merge_trees::RenameRenameConflict` |
| `pub_items_multiple_paths` | `id 14967: grit_lib::diff::merge_trees::TheirsConflictLabel | grit_lib::merge::merge_trees::TheirsConflictLabel` |
| `pub_items_multiple_paths` | `id 14969: grit_lib::diff::merge_trees::TreeMergeConflictPresentation | grit_lib::merge::merge_trees::TreeMergeConflictPresentation` |
| `pub_items_multiple_paths` | `id 14971: grit_lib::diff::merge_trees::TreeMergeOutput | grit_lib::merge::merge_trees::TreeMergeOutput` |
| `pub_items_multiple_paths` | `id 14982: grit_lib::diff::rerere::RerereAutoupdate | grit_lib::merge::RerereAutoupdate` |
| `pub_items_multiple_paths` | `id 14985: grit_lib::diff::rerere::RerereEventKind | grit_lib::merge::RerereEventKind` |
| `pub_items_multiple_paths` | `id 15595: grit_lib::objects::odb::WritableObjectStore | grit_lib::objects::odb::store::WritableObjectStore` |
| `pub_items_multiple_paths` | `id 16104: grit_lib::error::RepositoryError | grit_lib::repo::RepositoryError | grit_lib::repo::error::RepositoryError` |
| `pub_items_multiple_paths` | `id 16147: grit_lib::index::IndexError | grit_lib::index::error::IndexError` |
| `pub_items_multiple_paths` | `id 16384: grit_lib::remote::http_client::HttpClient | grit_lib::remote::transport::http::HttpClient` |
| `pub_items_multiple_paths` | `id 17008: grit_lib::error::PathspecFromFileParseError | grit_lib::worktree::pathspec::PathspecFromFileParseError` |
| `pub_items_multiple_paths` | `id 1711: grit_lib::merge::FastForwardPolicy | grit_lib::porcelain::merge::FastForwardPolicy` |
| `pub_items_multiple_paths` | `id 1746: grit_lib::merge::MergeCommitMode | grit_lib::porcelain::merge::MergeCommitMode` |
| `pub_items_multiple_paths` | `id 1782: grit_lib::diff::merge_file::MergeFavor | grit_lib::merge::merge_file::MergeFavor` |
| `pub_items_multiple_paths` | `id 1785: grit_lib::diff::merge_file::ConflictStyle | grit_lib::merge::merge_file::ConflictStyle` |
| `pub_items_multiple_paths` | `id 1791: grit_lib::diff::merge_trees::DirectoryRenameMode | grit_lib::merge::merge_trees::DirectoryRenameMode` |
| `pub_items_multiple_paths` | `id 1793: grit_lib::merge::MergeOptions | grit_lib::porcelain::merge::MergeOptions` |
| `pub_items_multiple_paths` | `id 1825: grit_lib::merge::IntegrateBranchOptions | grit_lib::porcelain::merge::IntegrateBranchOptions` |
| `pub_items_multiple_paths` | `id 1867: grit_lib::diff::merge_trees::WhitespaceMergeOptions | grit_lib::merge::merge_trees::WhitespaceMergeOptions` |
| `pub_items_multiple_paths` | `id 1871: grit_lib::merge::MergePredictionOptions | grit_lib::porcelain::merge::MergePredictionOptions` |
| `pub_items_multiple_paths` | `id 1902: grit_lib::merge::MergePrediction | grit_lib::porcelain::merge::MergePrediction` |
| `pub_items_multiple_paths` | `id 1935: grit_lib::merge::WrittenMergeTree | grit_lib::porcelain::merge::WrittenMergeTree` |
| `pub_items_multiple_paths` | `id 1936: grit_lib::merge::MergeTreeStage | grit_lib::porcelain::merge::MergeTreeStage` |
| `pub_items_multiple_paths` | `id 2007: grit_lib::merge::MergeDryRunPlan | grit_lib::porcelain::merge::MergeDryRunPlan` |
| `pub_items_multiple_paths` | `id 2041: grit_lib::merge::MergeSourceKind | grit_lib::porcelain::merge::MergeSourceKind` |
| `pub_items_multiple_paths` | `id 20441: grit_lib::objects::hash::ParallelHashError | grit_lib::parallel::ParallelHashError` |
| `pub_items_multiple_paths` | `id 2073: grit_lib::merge::MergeSource | grit_lib::porcelain::merge::MergeSource` |
| `pub_items_multiple_paths` | `id 2117: grit_lib::merge::IntegrateBranchOutcome | grit_lib::porcelain::merge::IntegrateBranchOutcome` |
| `pub_items_multiple_paths` | `id 2150: grit_lib::merge::ContinueOptions | grit_lib::porcelain::merge::ContinueOptions` |
| `pub_items_multiple_paths` | `id 2175: grit_lib::merge::ContinueOutcome | grit_lib::porcelain::merge::ContinueOutcome` |
| `pub_items_multiple_paths` | `id 23707: grit_lib::index::IndexCorruptReason | grit_lib::index::error::IndexCorruptReason` |
| `pub_items_multiple_paths` | `id 23742: grit_lib::index::IndexCorrupt | grit_lib::index::error::IndexCorrupt` |
| `pub_items_multiple_paths` | `id 23780: grit_lib::index::IndexChmodReason | grit_lib::index::error::IndexChmodReason` |
| `pub_items_multiple_paths` | `id 25311: grit_lib::objects::pack_objects::PackBuildOptions | grit_lib::remote::transfer::PackBuildOptions` |
| `pub_items_multiple_paths` | `id 2742: grit_lib::merge::ReplayDirection | grit_lib::porcelain::replay::ReplayDirection` |
| `pub_items_multiple_paths` | `id 2776: grit_lib::merge::ReplayRequest | grit_lib::porcelain::replay::ReplayRequest` |
| `pub_items_multiple_paths` | `id 2806: grit_lib::merge::ReplayOutcome | grit_lib::porcelain::replay::ReplayOutcome` |
| `pub_items_multiple_paths` | `id 28336: grit_lib::objects::odb::ObjectStore | grit_lib::objects::odb::store::ObjectStore` |
| `pub_items_multiple_paths` | `id 28339: grit_lib::objects::odb::ObjectStream | grit_lib::objects::odb::store::ObjectStream` |
| `pub_items_multiple_paths` | `id 2838: grit_lib::merge::ReplayEmptyPolicy | grit_lib::porcelain::replay_sequence::ReplayEmptyPolicy` |
| `pub_items_multiple_paths` | `id 28496: grit_lib::objects::pack_objects::build_pack | grit_lib::remote::transfer::build_pack` |
| `pub_items_multiple_paths` | `id 28498: grit_lib::objects::pack_objects::build_pack_from_send_list | grit_lib::remote::transfer::build_pack_from_send_list` |
| `pub_items_multiple_paths` | `id 28503: grit_lib::objects::pack_objects::PackObjects | grit_lib::remote::transfer::PackObjects` |
| `pub_items_multiple_paths` | `id 28505: grit_lib::objects::pack_objects::PackObjectsOptions | grit_lib::remote::transfer::PackObjectsOptions` |
| `pub_items_multiple_paths` | `id 28507: grit_lib::objects::pack_objects::PackStats | grit_lib::remote::transfer::PackStats` |
| `pub_items_multiple_paths` | `id 2878: grit_lib::merge::ReplayOptions | grit_lib::porcelain::replay_sequence::ReplayOptions` |
| `pub_items_multiple_paths` | `id 2907: grit_lib::merge::ReplaySequenceRequest | grit_lib::porcelain::replay_sequence::ReplaySequenceRequest` |
| `pub_items_multiple_paths` | `id 2933: grit_lib::merge::ReplayCreated | grit_lib::porcelain::replay_sequence::ReplayCreated` |
| `pub_items_multiple_paths` | `id 2972: grit_lib::merge::SequencePrediction | grit_lib::porcelain::replay_sequence::SequencePrediction` |
| `pub_items_multiple_paths` | `id 3017: grit_lib::merge::SequenceOutcome | grit_lib::porcelain::replay_sequence::SequenceOutcome` |
| `pub_items_multiple_paths` | `id 3049: grit_lib::merge::ReplayAbortOutcome | grit_lib::porcelain::replay_sequence::ReplayAbortOutcome` |
| `pub_items_multiple_paths` | `id 31970: grit_lib::refs::reflog::MessageMatcher | grit_lib::repo_handles::MessageMatcher` |
| `pub_items_multiple_paths` | `id 34046: grit_lib::remote::http_client::UreqHttpClient | grit_lib::remote::transport::http::ureq_client::UreqHttpClient` |
| `pub_items_multiple_paths` | `id 35104: grit_lib::remote::transport::http::http_client_arc | grit_lib::remote::transport::http_client_arc` |
| `pub_items_multiple_paths` | `id 35129: grit_lib::remote::redact_url_for_display | grit_lib::remote::transport_path::redact_url_for_display` |
| `pub_items_multiple_paths` | `id 35236: grit_lib::repo::UnknownRepositoryExtensions | grit_lib::repo::error::UnknownRepositoryExtensions` |
| `pub_items_multiple_paths` | `id 35269: grit_lib::repo::V1OnlyExtensionsInFormatZero | grit_lib::repo::error::V1OnlyExtensionsInFormatZero` |
| `pub_items_multiple_paths` | `id 35404: grit_lib::repo::RepositoryResult | grit_lib::repo::error::RepositoryResult` |
| `pub_items_multiple_paths` | `id 36912: grit_lib::repo_handles::LockWorktreeOptions | grit_lib::worktree::LockWorktreeOptions` |
| `pub_items_multiple_paths` | `id 3968: grit_lib::porcelain::status::UntrackedMode | grit_lib::status::UntrackedMode` |
| `pub_items_multiple_paths` | `id 4001: grit_lib::porcelain::status::IgnoredMode | grit_lib::status::IgnoredMode` |
| `pub_items_multiple_paths` | `id 4033: grit_lib::porcelain::status::RenameDetection | grit_lib::status::RenameDetection` |
| `pub_items_multiple_paths` | `id 4073: grit_lib::porcelain::status::StatusOptions | grit_lib::status::StatusOptions` |
| `pub_items_multiple_paths` | `id 4124: grit_lib::porcelain::status::InterruptedRewrite | grit_lib::status::InterruptedRewrite` |
| `pub_items_multiple_paths` | `id 4125: grit_lib::porcelain::status::StatusModel | grit_lib::status::StatusModel` |
| `pub_items_multiple_paths` | `id 4128: grit_lib::porcelain::status::ConflictGroup | grit_lib::status::ConflictGroup` |
| `pub_items_multiple_paths` | `id 4131: grit_lib::porcelain::status::StatusGuidance | grit_lib::status::StatusGuidance` |
| `pub_items_multiple_paths` | `id 4231: grit_lib::porcelain::status::StatusAction | grit_lib::status::StatusAction` |
| `pub_items_multiple_paths` | `id 4296: grit_lib::porcelain::status::UntrackedCollectMeta | grit_lib::status::UntrackedCollectMeta` |
| `pub_items_multiple_paths` | `id 4320: grit_lib::porcelain::status::format_porcelain_v2 | grit_lib::status::format_porcelain_v2` |
| `pub_items_multiple_paths` | `id 4401: grit_lib::porcelain::status::short_status_lines | grit_lib::status::short_status_lines` |
| `pub_items_multiple_paths` | `id 5394: grit_lib::refs::check_ref_format::RefName | grit_lib::refs::store::RefName` |
| `pub_items_multiple_paths` | `id 5433: grit_lib::revwalk::CommitFilterError | grit_lib::revwalk::filter::CommitFilterError` |
| `pub_items_multiple_paths` | `id 5457: grit_lib::revwalk::CommitFilter | grit_lib::revwalk::filter::CommitFilter` |
| `pub_items_multiple_paths` | `id 5489: grit_lib::revwalk::CommitFilterBuilder | grit_lib::revwalk::filter::CommitFilterBuilder` |
| `pub_items_multiple_paths` | `id 5742: grit_lib::revwalk::AuthorGroup | grit_lib::revwalk::group_by::AuthorGroup` |
| `pub_items_multiple_paths` | `id 5773: grit_lib::revwalk::GroupByAuthorOptions | grit_lib::revwalk::group_by::GroupByAuthorOptions` |
| `pub_items_multiple_paths` | `id 5800: grit_lib::revwalk::RevWalk | grit_lib::revwalk::handle::RevWalk` |
| `pub_items_multiple_paths` | `id 5823: grit_lib::revwalk::StdinRevisionSpecs | grit_lib::revwalk::handle::StdinRevisionSpecs` |
| `pub_items_multiple_paths` | `id 5831: grit_lib::revwalk::RevWalkCommitIter | grit_lib::revwalk::handle::RevWalkCommitIter` |
| `pub_items_multiple_paths` | `id 6281: grit_lib::diff::Pickaxe | grit_lib::revwalk::Pickaxe` |
| `pub_items_multiple_paths` | `id 6287: grit_lib::revwalk::CommitExtent | grit_lib::revwalk::rev_list::CommitExtent` |
| `pub_items_multiple_paths` | `id 6773: grit_lib::diff::PickaxeError | grit_lib::revwalk::PickaxeError` |
| `pub_items_multiple_paths` | `id 6775: grit_lib::diff::PickaxeKind | grit_lib::revwalk::PickaxeKind` |
| `pub_items_multiple_paths` | `id 798: grit_lib::commits::HookPolicy | grit_lib::porcelain::commit::HookPolicy` |
| `pub_items_multiple_paths` | `id 833: grit_lib::commits::CommitAuthorInfo | grit_lib::porcelain::commit::CommitAuthorInfo` |
| `pub_items_multiple_paths` | `id 892: grit_lib::commits::CommitRequest | grit_lib::porcelain::commit::CommitRequest` |
| `pub_items_multiple_paths` | `id 922: grit_lib::commits::CommitFile | grit_lib::porcelain::commit::CommitFile` |
| `pub_items_multiple_paths` | `id 9414: grit_lib::refs::check_ref_format::RefNameRef | grit_lib::refs::store::RefNameRef` |
| `pub_items_multiple_paths` | `id 956: grit_lib::commits::CommitPlan | grit_lib::porcelain::commit::CommitPlan` |
| `pub_items_multiple_paths` | `id 9602: grit_lib::remote::DEFAULT_REMOTE | grit_lib::remote::push_plan::DEFAULT_REMOTE` |
| `pub_items_multiple_paths` | `id 991: grit_lib::commits::CommitPatchInput | grit_lib::porcelain::commit::CommitPatchInput` |
| `pub_items_multiple_paths` | `id 994: grit_lib::commits::CommitScope | grit_lib::porcelain::commit::CommitScope` |
| `pub_progress_interfaces` | `grit_lib::fetch::NoProgress :: struct NoProgress` |
| `pub_progress_interfaces` | `grit_lib::fetch::Progress :: trait Progress` |
| `pub_progress_interfaces` | `grit_lib::progress::NullProgress :: struct NullProgress` |
| `pub_progress_interfaces` | `grit_lib::progress::ProgressSink :: trait ProgressSink` |
| `pub_repo_constructor_offenders` | `grit_lib::repo::Repository::discover :: discover` |
| `pub_repo_constructor_offenders` | `grit_lib::repo::Repository::open :: open` |
| `pub_repo_constructor_offenders` | `grit_lib::repo::Repository::with_ref_store :: with_ref_store` |
| `pub_repo_constructor_offenders` | `with_thread_policy (grit-lib/src/repo.rs:532) :: with_thread_policy` |
<!-- api-audit:end -->
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

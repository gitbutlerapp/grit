# Typed errors in grit-lib

Grit-lib returns **structured errors**, not strings. Every fallible library call returns [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result), whose error is the umbrella [`Error`](rustdoc:grit_lib::error::Error). The umbrella has **one variant per functional area**, and each area variant carries that area's own `#[non_exhaustive]` `thiserror` enum (see the [error contract](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/API.md#4-typed-errors-per-area) in `API.md`).

## Matching an error

Match the area first, then the area's variant:

```rust
use grit_lib::error::{Error, RefError, RevisionError};

fn describe(err: &Error) -> &'static str {
    match err {
        Error::Revision(RevisionError::NotFound { .. }) => "no such revision",
        Error::Ref(RefError::UnbornHead) => "no commits yet",
        Error::LockHeld(_) => "another process holds a lock",
        err if err.is_cancelled() => "cancelled",
        _ => "something else",
    }
}
```

Both [`Error`](rustdoc:grit_lib::error::Error) and every area enum are `#[non_exhaustive]`, so a match always ends in a wildcard arm. Area enums convert into the umbrella with `From`, so `?` works across areas inside your own code.

## Rules

- Match on **types and variants**, not message text. Human wording is for the CLI only.
- No catch-all `Message(String)` variant anywhere; no `Result<_, String>` in the public API.
- Named fields only: `PathBuf`, object ids, ref names, reason enums. A bare `String` field is allowed only for **external data** (for example a remote's error line) and must be a **named** field, not a tuple variant.
- Use [`PathIoError`](rustdoc:grit_lib::error::PathIoError) for path-related I/O; do not add bare `#[from] io::Error` on area enums.
- Area enums never hold another umbrella `Error`. A failure in a lower area surfaces as that area's own umbrella variant (a missing object during a checkout is [`Error::Object`](rustdoc:grit_lib::error::Error::Object), not a checkout error that wraps it).
- When a variant must report a *secondary* failure next to its own (a rollback that also failed, one promisor remote's failure among several), it keeps an [`ErrorSummary`](rustdoc:grit_lib::error::ErrorSummary) of it: the inner failure's `kind`, `area`, message and whether it was a cancellation.
- Corrupt loose/packed reads use [`ObjectReadFailure`](rustdoc:grit_lib::error::ObjectReadFailure) as the `source` of [`ObjectError::CorruptLoose`](rustdoc:grit_lib::error::ObjectError::CorruptLoose) / [`ObjectError::CorruptPacked`](rustdoc:grit_lib::error::ObjectError::CorruptPacked).
- Public functions return `grit_lib::error::Result<T>` (or a small leaf error for pure parsers), not per-area type aliases.

## Shared leaf types

| Type | Role |
| --- | --- |
| [`LockHeldError`](rustdoc:grit_lib::error::LockHeldError) | Lock file exists; optional [`LockHolder`](rustdoc:grit_lib::lockfile::LockHolder) from the pid sidecar ([`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld)) |
| [`PathIoError`](rustdoc:grit_lib::error::PathIoError) | Worktree or git-dir path I/O with [`PathIoOperation`](rustdoc:grit_lib::error::PathIoOperation); read it back with [`Error::path_io`](rustdoc:grit_lib::error::Error::path_io) |
| [`Cancelled`](rustdoc:grit_lib::error::Cancelled) | Operation stopped via [`CancelToken`](rustdoc:grit_lib::progress::CancelToken) ([`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled); test with [`Error::is_cancelled`](rustdoc:grit_lib::error::Error::is_cancelled)) |
| [`ErrorSummary`](rustdoc:grit_lib::error::ErrorSummary) | Kind, area and message of a secondary failure inside another error |
| [`ObjectReadFailure`](rustdoc:grit_lib::error::ObjectReadFailure) | Non-recursive decode failure for corrupt loose/packed object reads |

## Area map

| Umbrella variant | Area enum | Covers |
| --- | --- | --- |
| [`Error::Repository`](rustdoc:grit_lib::error::Error::Repository) | [`RepositoryError`](rustdoc:grit_lib::repo::RepositoryError) | open, init, layout, ownership, git-dir path I/O |
| [`Error::Object`](rustdoc:grit_lib::error::Error::Object) | [`ObjectError`](rustdoc:grit_lib::error::ObjectError) | object ids, ODB, packs, MIDX, bitmaps, promisor fetches |
| [`Error::Index`](rustdoc:grit_lib::error::Error::Index) | [`IndexError`](rustdoc:grit_lib::index::IndexError) | corrupt bytes, unmerged paths, fsmonitor/cache-tree extensions, split index |
| [`Error::Ref`](rustdoc:grit_lib::error::Error::Ref) | [`RefError`](rustdoc:grit_lib::error::RefError) | ref names, `HEAD` state, ref store, reftable, reflog |
| [`Error::Revision`](rustdoc:grit_lib::error::Error::Revision) | [`RevisionError`](rustdoc:grit_lib::error::RevisionError) | rev-parse, rev-list, merge bases |
| [`Error::Config`](rustdoc:grit_lib::error::Error::Config) | [`ConfigError`](rustdoc:grit_lib::error::ConfigError) | config parsing, includes, writes |
| [`Error::Worktree`](rustdoc:grit_lib::error::Error::Worktree) | [`WorktreeError`](rustdoc:grit_lib::worktree::WorktreeError) | checkout, status, add/rm/mv/restore/clean, pathspec, filters, submodules, linked worktrees |
| [`Error::Diff`](rustdoc:grit_lib::error::Error::Diff) | [`DiffError`](rustdoc:grit_lib::error::DiffError) | diff, blame, apply, hunk selection, textconv |
| [`Error::Commit`](rustdoc:grit_lib::error::Error::Commit) | [`CommitError`](rustdoc:grit_lib::porcelain::CommitError) | messages, identity, signing, change ids |
| [`Error::Merge`](rustdoc:grit_lib::error::Error::Merge) | [`MergeError`](rustdoc:grit_lib::porcelain::MergeError) | merge, pick, revert, sequencer, operation state, pull |
| [`Error::History`](rustdoc:grit_lib::error::Error::History) | [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError) | history rewrites, rebase sessions, undo |
| [`Error::Stash`](rustdoc:grit_lib::error::Error::Stash) | [`StashError`](rustdoc:grit_lib::stash::StashError) | stash push, apply, drop |
| [`Error::Branch`](rustdoc:grit_lib::error::Error::Branch) | [`BranchError`](rustdoc:grit_lib::porcelain::branch::BranchError) | branch create, delete, rename, switch targets |
| [`Error::Tag`](rustdoc:grit_lib::error::Error::Tag) | [`TagError`](rustdoc:grit_lib::porcelain::tag::TagError) | tag create, delete, verify |
| [`Error::Notes`](rustdoc:grit_lib::error::Error::Notes) | [`NotesError`](rustdoc:grit_lib::error::NotesError) | notes refs and edits |
| [`Error::Remote`](rustdoc:grit_lib::error::Error::Remote) | [`RemoteError`](rustdoc:grit_lib::remote::RemoteError) | transports, fetch, push, clone, bundles, credentials, serve, snapshot checkouts |
| [`Error::Hook`](rustdoc:grit_lib::error::Error::Hook) | [`HookError`](rustdoc:grit_lib::command_runner::hooks::HookError) | hooks that fail or cannot start |
| [`Error::Io`](rustdoc:grit_lib::error::Error::Io) | `std::io::Error` | I/O not tied to a repository path |
| [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld) | [`LockHeldError`](rustdoc:grit_lib::error::LockHeldError) | lock contention in any area |
| [`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled) | — | cancellation in any area |

Fold tiny areas into a neighbour; do not add an umbrella variant for only one or two failures.

## JSON and scripts

[`Error::kind_name`](rustdoc:grit_lib::error::Error::kind_name) returns a stable snake_case slug for the specific failure (for example `revision_not_found` or `unborn_head`); each area enum keeps the slugs its variants had before the umbrella collapsed. [`Error::area`](rustdoc:grit_lib::error::Error::area) returns the area (`"revision"`, `"ref"`, …). With the `serde-models` feature a serialized error carries both, as `kind` and the additive `area` field. See [Embedding — Typed errors](/docs/library/embedding#typed-errors).

## Ratchet

Quality counts in [`api-ratchet.toml`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/api-ratchet.toml) track the umbrella variant count, catch-all variants, string tuples, missing `#[non_exhaustive]`, area enums that hold an umbrella `Error`, and `Error::Message(` construction sites under `grit-lib/src`. The catch-all, string, `non_exhaustive`, nesting and construction-site counts are all zero; `make api-ratchet-check` in CI keeps them there.

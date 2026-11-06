# Typed errors in grit-lib

Grit-lib returns **structured errors**, not strings. Each functional area gets its own `#[non_exhaustive]` `thiserror` enum; the legacy umbrella [`Error`](rustdoc:grit_lib::error::Error) shrinks as those enums grow (see the [error contract](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/API.md#4-typed-errors-per-area) in `API.md`).

## Rules

- Match on **types and variants**, not message text. Human wording is for the CLI only.
- No catch-all `Message(String)` on area enums; no `Result<_, String>` in the public API.
- Named fields only: `PathBuf`, object ids, ref names, reason enums. A bare `String` field is allowed only for **external data** (for example a remote's error line) and must be a **named** field, not a tuple variant.
- Use [`PathIoError`](rustdoc:grit_lib::error::PathIoError) for path-related I/O; do not add bare `#[from] io::Error` on area enums.
- Cross-area failures wrap the lower area's enum. Temporary `Nested(Box<Error>)` on the umbrella is allowed until the final collapse step.
- Public functions return `Result<T, AreaError>`, not per-area type aliases.

## Shared leaf types

| Type | Role |
| --- | --- |
| [`LockHeldError`](rustdoc:grit_lib::error::LockHeldError) | Lock file exists; optional [`LockHolder`](rustdoc:grit_lib::lockfile::LockHolder) from the pid sidecar |
| [`PathIoError`](rustdoc:grit_lib::error::PathIoError) | Worktree or git-dir path I/O with [`PathIoOperation`](rustdoc:grit_lib::error::PathIoOperation) |
| [`Cancelled`](rustdoc:grit_lib::error::Cancelled) | Operation stopped via [`CancelToken`](rustdoc:grit_lib::progress::CancelToken) |

## Area map (target)

| Area | Error enum | Primary modules |
| --- | --- | --- |
| Repository | `RepositoryError` | `repo` (open, init, layout) |
| Config | [`ConfigError`](rustdoc:grit_lib::error::ConfigError) | `config` |
| Hook | `HookError` | `command_runner`, hooks |
| Object | `ObjectError` | `objects`, ODB, packs, MIDX, commit-graph, fsck, maintenance |
| Ref | `RefError` | `refs`, reflog, packed, reftable |
| Branch | `BranchError` | porcelain branch |
| Tag | `TagError` | porcelain tag |
| Notes | `NotesError` | `notes` |
| Index | `IndexError` | `index` |
| Worktree | `WorktreeError` | checkout, status, add/rm/mv/restore/clean, pathspec, attributes, filters, submodules, linked worktrees, hunk selection |
| Diff | `DiffError` | `diff`, blame, apply, rerere |
| Revision | `RevisionError` | rev-parse, rev-list, merge-base, object filters |
| Commit | `CommitError` | messages, identity, signing |
| Merge | `MergeError` | merge, pick, revert, sequencer, operation state |
| History | `HistoryError` | `revwalk::history` |
| Stash | `StashError` | `stash` |
| Remote | `RemoteError` | transport, fetch, push, clone, credentials, serve |
| Bundle | `BundleError` | `bundle` |

Fold tiny areas into a neighbour; do not add an enum for only one or two variants.

## JSON and scripts

[`Error::kind_name`](rustdoc:grit_lib::error::Error::kind_name) slugs are stable per variant rename. Serialized errors keep the existing `kind` byte; an additive `area` field may appear later. See [Embedding — Typed errors](/docs/library/embedding#typed-errors).

## Ratchet

Quality counts in [`api-ratchet.toml`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/api-ratchet.toml) track umbrella variant count, catch-all variants, string tuples, missing `#[non_exhaustive]`, nested `Error` wraps, and `Error::Message(` construction sites under `grit-lib/src`. Run `make api-ratchet-check` in CI.

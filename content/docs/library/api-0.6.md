---
title: grit-lib 0.6 API overview
summary: One-page tour of Repository handles, options, progress, errors, extension points, and the 0.6.x stability promise.
---

This page is the embedder's **first read** for the supported grit-lib surface on the path to **0.6.0**. It summarizes how to open a repository, which handles own which work, how optional behavior and long operations are shaped, how errors are typed, and where you plug in custom storage or subprocesses. For the seven API conventions and semver rules in full, see [`grit-lib/API.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/API.md). For canonical module paths, see [API layout](api-layout.md).

## Entry points

[`Repository`](rustdoc:grit_lib::repo::Repository) is the single entry handle. Obtain it in one of three ways:

| Method | When to use |
| --- | --- |
| [`Repository::discover`](rustdoc:grit_lib::repo::Repository::discover) | Walk upward from a working directory (or `None` for the process cwd), like `git` discovery. |
| [`Repository::open`](rustdoc:grit_lib::repo::Repository::open) | You already know the git directory path; set the work tree on [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions). |
| [`Repository::init`](rustdoc:grit_lib::repo::Repository::init) | Create a new repository; returns `(Repository, InitOutcome)` describing what was written. |

[`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) bundles everything needed besides paths:

- [`RepositoryOptions::from_process`](rustdoc:grit_lib::config::environment::RepositoryOptions::from_process) — snapshot the calling process so system/global config and `HOME` behave like `git` and `grit` on the user's machine.
- [`RepositoryOptions::empty`](rustdoc:grit_lib::config::environment::RepositoryOptions::empty) — hermetic opens for tests and tools that own their own config.
- [`RepositoryOptions::with_environment`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_environment) — an explicit [`Environment`](rustdoc:grit_lib::config::environment::Environment) you built.
- [`RepositoryOptions::with_object_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_object_store) and [`RepositoryOptions::with_odb_builder`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_odb_builder) — custom object storage and read layers (see [Object stores](object-stores.md)).
- [`RepositoryOptions::with_ref_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_ref_store) — replace loose/packed/reftable refs on disk.

Pass options into init with [`InitOptions::with_repository_options`](rustdoc:grit_lib::repo::InitOptions::with_repository_options). Deeper discussion of discovery, ownership, and embedding lives in [Repository](repository.md) and [Embedding grit-lib](embedding.md).

## Handle map

An operation on a repository is a **method on the handle that owns its area**, not a free function taking `&Repository`. Call `repo.<accessor>()` once, then methods on the returned handle.

| Accessor | Handle | Owns |
| --- | --- | --- |
| [`objects`](rustdoc:grit_lib::repo::Repository::objects) | [`Objects`](rustdoc:grit_lib::objects::Objects) | Read/write objects, build trees and commits, pack-aware reads |
| [`maintenance`](rustdoc:grit_lib::repo::Repository::maintenance) | [`Maintenance`](rustdoc:grit_lib::maintenance::Maintenance) | gc, repack, prune, commit-graph and MIDX maintenance, fsck, pack-refs |
| [`references`](rustdoc:grit_lib::repo::Repository::references) | [`References`](rustdoc:grit_lib::refs::References) | Resolve, list, update refs; branches and tags; ref transactions |
| [`reflogs`](rustdoc:grit_lib::repo::Repository::reflogs) | [`RepositoryReflogs`](rustdoc:grit_lib::repo_handles::RepositoryReflogs) | Reflog read, append, expire |
| [`operation`](rustdoc:grit_lib::repo::Repository::operation) | [`Operations`](rustdoc:grit_lib::operation::Operations) | Paused merge, rebase, pick, revert, bisect state |
| [`index`](rustdoc:grit_lib::repo::Repository::index) | [`IndexFile`](rustdoc:grit_lib::index::IndexFile) | Load/write the index, stage paths, sparse checkout |
| [`config`](rustdoc:grit_lib::repo::Repository::config) | [`Config`](rustdoc:grit_lib::config::Config) | Merged config get; local set and reload |
| [`revwalk`](rustdoc:grit_lib::repo::Repository::revwalk) | [`RevWalk`](rustdoc:grit_lib::revwalk::RevWalk) | Rev-parse, rev-list, merge-base, log-style walks |
| [`diff`](rustdoc:grit_lib::repo::Repository::diff) | [`Diff`](rustdoc:grit_lib::diff::Diff) | Tree, index, and worktree diffs; blame; patch-ids; apply |
| [`remotes`](rustdoc:grit_lib::repo::Repository::remotes) | [`RepositoryRemotes`](rustdoc:grit_lib::repo_handles::RepositoryRemotes) | Remote config, fetch, push, ls-remote, caller-supplied transport |
| [`bundles`](rustdoc:grit_lib::repo::Repository::bundles) | [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Bundle create, verify, unbundle |
| [`notes`](rustdoc:grit_lib::repo::Repository::notes) | [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes) | Notes read, add, remove |
| [`stash`](rustdoc:grit_lib::repo::Repository::stash) | [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) | Stash push, apply, pop, list |
| [`hooks`](rustdoc:grit_lib::repo::Repository::hooks) | [`RepositoryHooks`](rustdoc:grit_lib::repo_handles::RepositoryHooks) | Hook lookup and dispatch |
| [`worktrees`](rustdoc:grit_lib::repo::Repository::worktrees) | [`RepositoryWorktrees`](rustdoc:grit_lib::repo_handles::RepositoryWorktrees) | Linked worktrees add, list, lock, remove, prune |
| [`workspace_snapshot`](rustdoc:grit_lib::repo::Repository::workspace_snapshot) | [`RepositoryWorkspaceSnapshot`](rustdoc:grit_lib::repo_handles::RepositoryWorkspaceSnapshot) | GitButler workspace snapshots |
| [`merge`](rustdoc:grit_lib::repo::Repository::merge) | [`Merge`](rustdoc:grit_lib::merge::Merge) | Merge, pick, revert, replay, conflict check, rerere |
| [`history`](rustdoc:grit_lib::repo::Repository::history) | [`History`](rustdoc:grit_lib::revwalk::history::History) | Commits, amends, history edits, rebase sessions |

For the raw ref backend trait object, use [`references().store()`](rustdoc:grit_lib::refs::References::store) (same as [`Repository::ref_store`](rustdoc:grit_lib::repo::Repository::ref_store)). Topic guides in [this section](index.md) walk each area with longer examples.

## Options structs

Optional behavior belongs in one **`#[non_exhaustive]` options struct** per operation, with `Default` and builder-style `with_<field>` setters on that struct — not `_with_x_and_y` function families and not more than one bare `bool` on a public function.

- Prefer enums for named choices (`RepackScope::All`, not `all: bool`).
- The options struct is the **last value parameter** before progress and cancellation (when those apply).

Examples appear throughout the guides: [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions), [`IndexLoadOptions`](rustdoc:grit_lib::index::IndexLoadOptions), [`RepackOptions`](rustdoc:grit_lib::maintenance::RepackOptions), [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions), and [`InitOptions`](rustdoc:grit_lib::repo::InitOptions).

## Progress and cancellation

Long-running work takes a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink) and a [`CancelToken`](rustdoc:grit_lib::progress::CancelToken) as **trailing parameters**, in that order, after the options struct:

```rust
repo.maintenance().repack(
    &RepackOptions::default(),
    &mut NullProgress,
    &CancelToken::never(),
)?;
```

Callers that do not need updates or interruption pass [`NullProgress`](rustdoc:grit_lib::progress::NullProgress) and [`CancelToken::never`](rustdoc:grit_lib::progress::CancelToken::never). Progress and cancel are not fields on options structs, and there are no duplicate `_with_progress` entry points. See [Parallelism](parallelism.md) for thread policy and where cancellation surfaces as [`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled).

## Errors

Fallible APIs return [`grit_lib::error::Result`](rustdoc:grit_lib::error::Result). The umbrella [`Error`](rustdoc:grit_lib::error::Error) has **one variant per functional area**, each wrapping that area's `#[non_exhaustive]` `thiserror` enum. Match on variants, not message strings:

```rust
use grit_lib::error::{Error, ObjectError, RefError};

match err {
    Error::Object(ObjectError::NotFound { .. }) => { /* ... */ }
    Error::Ref(RefError::UnbornHead) => { /* ... */ }
    Error::LockHeld(_) => { /* ... */ }
    e if e.is_cancelled() => { /* ... */ }
    _ => { /* non_exhaustive arms */ }
}
```

Shared leaf types used across areas:

| Type | Role |
| --- | --- |
| [`PathIoError`](rustdoc:grit_lib::error::PathIoError) | Path-related I/O with [`PathIoOperation`](rustdoc:grit_lib::error::PathIoOperation) |
| [`LockHeldError`](rustdoc:grit_lib::error::LockHeldError) | Another process holds a lock ([`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld)) |
| [`Cancelled`](rustdoc:grit_lib::error::Cancelled) | [`CancelToken`](rustdoc:grit_lib::progress::CancelToken) fired ([`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled)) |
| [`ErrorSummary`](rustdoc:grit_lib::error::ErrorSummary) | Secondary failure bundled inside another variant (rollback, per-remote errors) |

The full area map and CLI mapping notes are in [Typed errors](errors.md).

## Extension points

| Extension | Trait / type | Role |
| --- | --- | --- |
| Object store | [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore), [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore) | Pluggable ODB primary and read layers via [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) |
| Ref store | [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) | Backends: [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore), packed files layout, [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore), [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore) |
| Subprocesses | [`CommandRunner`](rustdoc:grit_lib::command_runner::CommandRunner) | Hooks, smudge/clean filters, credentials, signing; default [`SystemCommandRunner`](rustdoc:grit_lib::command_runner::SystemCommandRunner) |
| Warnings and trace | [`DiagnosticSink`](rustdoc:grit_lib::diagnostics::DiagnosticSink) | Non-fatal [`Warning`](rustdoc:grit_lib::diagnostics::Warning) values; default [`NullDiagnostics`](rustdoc:grit_lib::diagnostics::NullDiagnostics) on [`RepositoryOptions::diagnostics`](rustdoc:grit_lib::config::environment::RepositoryOptions) |

Implementing a backend end-to-end (conformance tests, init without `objects/`, performance hooks) is covered in [Storage backends](storage-backends.md) and [Object stores](object-stores.md).

## Outside semver

The **`test-support`** cargo feature on `grit-lib` exposes fixtures, fuzz entry points, and bench helpers for the Grit workspace and CI. It is **not** part of the supported public API and is not subject to semver checks. Downstream crates should not enable it in production dependencies.

## Stability promise

- **Until 0.6.0:** breaking changes are acceptable when they **consolidate** the API (fewer, clearer entry points following [`API.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/API.md)).
- **From 0.6.0 through 0.6.x:** the public API only **grows**. Removals, renames, and signature changes keep the old form alongside, mark it `#[deprecated(note = "use …")]`, and remove it in **0.7**.
- **Enforcement:** `make semver-check` runs [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks) against the published baseline; intentional surface changes update [`grit-lib/public-api.txt`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/public-api.txt) via `make public-api`.

## Example

The program below opens a temporary repository, writes and reads a blob, lists branch refs, runs a worktree diff with an options struct, and matches a missing-object error. It is built from the repository on every docs CI run.

<!-- include: grit-examples/examples/api_overview.rs -->

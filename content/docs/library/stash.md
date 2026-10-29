---
title: Stash
summary: Stash stack read/write through RepositoryStash, with option and result types in porcelain::stash.
---

The stash stack is Git-compatible ref and commit machinery. There is one API for it: `Repository::stash` returns a [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) handle with `list`, `push`, `preview`, `create`, `apply`, `apply_commit`, `pop`, `drop`, `clear`, `diff` and `branch`. The option and result types live in [`porcelain::stash`](rustdoc:grit_lib::porcelain::stash) (also re-exported as [`stash`](rustdoc:grit_lib::stash)). Entries are addressed by their newest-first index (`0` is `stash@{0}`), and the handle works on the repository's own work tree, so there is no separate work-tree argument.

## Push and list

[`StashPushOptions`](rustdoc:grit_lib::porcelain::stash::StashPushOptions) carries the message, whether to include untracked files, [`StashMode`](rustdoc:grit_lib::porcelain::stash::StashMode) (`All`, `StagedOnly`, `KeepIndex`), optional hunk selection and pathspecs, and the [`Signature`](rustdoc:grit_lib::ident::Signature) used for the stash commits and the `refs/stash` reflog (usually `repo.identity(IdentRole::Committer, now)`, see [Repository](../repository/#identity)). [`StashPushOptions::new`](rustdoc:grit_lib::porcelain::stash::StashPushOptions::new) sets the other fields to their defaults. `RepositoryStash::push` returns a [`StashPushResult`](rustdoc:grit_lib::porcelain::stash::StashPushResult) (the stash commit, its message and tree), or `Ok(None)` when there is nothing to stash. `RepositoryStash::preview` takes the same options and lists the paths that would be saved without writing objects or refs. For a partial push (a hunk selection), both the result and the [`StashPreview`](rustdoc:grit_lib::porcelain::stash::StashPreview) report what the selection resolved to: `stashed_hunks` maps each path to the hunk ids it stashes and `stashed_lines` to its line range labels, so callers do not need to resolve the selection again. `StashPushOptions::validate` rejects a selection combined with `include_untracked`, `StagedOnly` or `KeepIndex` as [`Error::StashOptionConflict`](rustdoc:grit_lib::error::Error::StashOptionConflict). Stash commits are always written unsigned, whatever `commit.gpgsign` says, matching Git.

`push`, `apply`, `apply_commit` and `pop` take a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink) and a [`Cancel`](rustdoc:grit_lib::progress::Cancel). Push reports [`StagingFiles`](rustdoc:grit_lib::progress::ProgressPhase::StagingFiles) while it hashes local changes and [`CheckingOutFiles`](rustdoc:grit_lib::progress::ProgressPhase::CheckingOutFiles) while it resets them. A cancel before `refs/stash` moves returns [`Error::Cancelled`](rustdoc:grit_lib::error::Error::Cancelled) with the repository unchanged; once the entry is stored it stays, even if the reset that follows is cancelled. A cancelled apply or pop keeps the entry, leaves the index alone and rolls its checkout back; only files it content-merged onto a moved `HEAD` before the cancel stay written.

## Apply and pop

`RepositoryStash::apply` applies `stash@{n}` and keeps the entry. `RepositoryStash::pop` applies it and drops it only when it applied cleanly. Both take one [`StashApplyOptions`](rustdoc:grit_lib::porcelain::stash::StashApplyOptions) (`restore_index` restores the stashed index, like `git stash apply --index`; `overwrite_identical_untracked` skips untracked collisions when on-disk bytes already match the stashed copy) and return a [`StashApplyOutcome`](rustdoc:grit_lib::porcelain::stash::StashApplyOutcome) whose `conflicts` lists each path left with conflict stages in the index as an [`operation::Conflict`](rustdoc:grit_lib::operation::Conflict), the same paths `git ls-files -u` shows afterwards, with the same [`ConflictKind`](rustdoc:grit_lib::operation::ConflictKind) `status` reports for them. `has_conflicts()` is true exactly when a pop kept the entry. `RepositoryStash::apply_commit` applies any stash-shaped commit, such as one built by `RepositoryStash::create`, without touching `refs/stash`. An index past the end of the stack is [`Error::StashNotFound`](rustdoc:grit_lib::error::Error::StashNotFound).

Dropping an entry rewrites the `refs/stash` reflog without adding a new entry, so `drop`, `pop` and `branch` take no identity.

```rust
use grit_lib::porcelain::stash::StashApplyOptions;
use grit_lib::progress::{NeverCancel, NullProgress};

let popped = repo
    .stash()
    .pop(0, &StashApplyOptions::default(), &mut NullProgress, &NeverCancel)?;
for conflict in &popped.conflicts {
    println!(
        "conflict: {} ({}; stash@{{0}} was kept)",
        conflict.path,
        conflict.kind.as_str()
    );
}
```

## Example

<!-- include: grit-examples/src/bin/guide_stash.rs -->

After a dirty file and `cargo run --bin guide_stash`, `git stash list` must show the entry. The test also runs `git fsck --strict`.

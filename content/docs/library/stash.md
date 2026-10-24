---
title: Stash
summary: Stash stack read/write through RepositoryStash, with option and result types in grit_lib::stash.
---

The stash stack is Git-compatible ref and commit machinery. There is one API for it: `Repository::stash` returns a [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) handle with `list`, `push`, `preview`, `create`, `apply`, `apply_commit`, `pop`, `drop`, `clear`, `diff` and `branch`. The option and result types live in [`stash`](rustdoc:grit_lib::stash). Entries are addressed by their newest-first index (`0` is `stash@{0}`), and the handle works on the repository's own work tree, so there is no separate work-tree argument.

## Push and list

[`StashPushOptions`](rustdoc:grit_lib::stash::StashPushOptions) carries the message, whether to include untracked files, [`StashMode`](rustdoc:grit_lib::stash::StashMode) (`All`, `StagedOnly`, `KeepIndex`), optional hunk selection and pathspecs, and the identity line used for the stash commits and the `refs/stash` reflog. `RepositoryStash::push` returns a [`StashPushResult`](rustdoc:grit_lib::stash::StashPushResult) (the stash commit, its message and tree), or `Ok(None)` when there is nothing to stash. `RepositoryStash::preview` takes the same options and lists the paths that would be saved without writing objects or refs.

## Apply and pop

`RepositoryStash::apply` applies `stash@{n}` and keeps the entry. `RepositoryStash::pop` applies it and drops it only when it applied cleanly. Both take one [`StashApplyOptions`](rustdoc:grit_lib::stash::StashApplyOptions) (`restore_index` restores the stashed index, like `git stash apply --index`) and return a [`StashApplyOutcome`](rustdoc:grit_lib::stash::StashApplyOutcome) whose `conflicted_paths` lists the sorted paths left with conflict stages in the index, the same paths `git ls-files -u` shows afterwards. `has_conflicts()` is true exactly when a pop kept the entry. `RepositoryStash::apply_commit` applies any stash-shaped commit, such as one built by `RepositoryStash::create`, without touching `refs/stash`. An index past the end of the stack is [`Error::StashNotFound`](rustdoc:grit_lib::error::Error::StashNotFound).

Dropping an entry rewrites the `refs/stash` reflog without adding a new entry, so `drop`, `pop` and `branch` take no identity.

```rust
use grit_lib::stash::StashApplyOptions;

let popped = repo.stash().pop(0, &StashApplyOptions::default())?;
for path in &popped.conflicted_paths {
    println!("conflict: {path} (stash@{{0}} was kept)");
}
```

## Example

<!-- include: grit-examples/src/bin/guide_stash.rs -->

After a dirty file and `cargo run --bin guide_stash`, `git stash list` must show the entry. The test also runs `git fsck --strict`.

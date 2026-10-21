---
title: Stash
summary: Stash stack read/write through RepositoryStash and porcelain stash types.
---

The stash stack is Git-compatible ref and commit machinery under [`stash`](rustdoc:grit_lib::stash) and `porcelain::stash`. For embedders, `Repository::stash` returns `RepositoryStash`: push, list, apply, pop, and drop with explicit author/committer identity strings (no hidden wall-clock reads in the handle layer).

## Push and list

`StashPushOptions` carries the message, whether to include untracked files, and the identity line used for stash commits. `RepositoryStash::push` returns `Ok(None)` when there is nothing to stash.

## Apply and pop

[`apply_stash_entry`](rustdoc:grit_lib::porcelain::stash::apply_stash_entry) applies `stash@{n}` and keeps the entry. [`pop_stash_entry`](rustdoc:grit_lib::porcelain::stash::pop_stash_entry) applies it and drops it only when it applied cleanly. Both return a [`StashApplyOutcome`](rustdoc:grit_lib::porcelain::stash::StashApplyOutcome) whose `conflicted_paths` lists the sorted paths left with conflict stages in the index, the same paths `git ls-files -u` shows afterwards. An index past the end of the stack is [`Error::StashNotFound`](rustdoc:grit_lib::error::Error::StashNotFound).

```rust
use grit_lib::porcelain::stash::pop_stash_entry;

let popped = pop_stash_entry(&repo, work_tree, 0, false, &reflog_identity)?;
for path in &popped.conflicted_paths {
    println!("conflict: {path} (stash@{{0}} was kept)");
}
```

## Example

<!-- include: grit-examples/src/bin/guide_stash.rs -->

After a dirty file and `cargo run --bin guide_stash`, `git stash list` must show the entry. The test also runs `git fsck --strict`.

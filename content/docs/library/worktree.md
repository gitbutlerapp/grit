---
title: Worktree
summary: Linked worktrees, ignore rules, attributes, and pathspec helpers under the worktree domain.
---

Working-tree behavior lives under [`worktree`](rustdoc:grit_lib::worktree): ignore/attributes (`worktree::ignore`, `worktree::attributes`), pathspecs (`worktree::pathspec`), and checkout helpers. Porcelain commands (`status`, `add`, `checkout`, …) compose these modules with the index and ODB.

## Linked worktrees (handle)

`Repository::worktrees` returns `RepositoryWorktrees`: list main and linked checkouts, add/remove/lock linked worktrees with the same on-disk layout Git uses (`worktrees/` admin files, `gitdir` links). Options such as `AddLinkedWorktreeOptions` mirror the fields the CLI exposes.

## Example

This program lists every worktree path grit sees for an open repository:

<!-- include: grit-examples/src/bin/guide_worktree.rs -->

Create a linked worktree with Git, then compare counts:

```bash
git worktree add ../linked -b feature
cargo run --bin guide_worktree .
git worktree list
```

The integration test adds a linked worktree with system Git and compares `worktree_count` to `git worktree list --porcelain`.

---
title: Worktree
summary: Linked worktrees, ignore rules, attributes, and pathspec helpers under the worktree domain.
---

Working-tree behavior lives under [`worktree`](rustdoc:grit_lib::worktree): ignore/attributes (`worktree::ignore`, `worktree::attributes`), pathspecs (`worktree::pathspec`), and checkout helpers. Porcelain commands (`status`, `add`, `checkout`, …) compose these modules with the index and ODB.

## Linked worktrees (handle)

`Repository::worktrees` returns `RepositoryWorktrees`: list main and linked checkouts, add/remove/move/repair/lock linked worktrees with the same on-disk layout Git uses (`worktrees/` admin files, `gitdir` links). [`AddLinkedWorktreeOptions`](rustdoc:grit_lib::worktree::AddLinkedWorktreeOptions) uses [`WorktreeBranch`](rustdoc:grit_lib::worktree::WorktreeBranch) for branch modes (`Create`, `CreateOrReset`, `Existing`, `Detach`), optional lock-before-checkout, `--no-checkout`, and `--track`. [`move_linked_worktree`](rustdoc:grit_lib::worktree::move_linked_worktree) and [`repair_worktrees`](rustdoc:grit_lib::worktree::repair_worktrees) rewrite gitfile links atomically; repairs return [`WorktreeRepair`](rustdoc:grit_lib::worktree::WorktreeRepair) rows. [`prune_linked_worktrees`](rustdoc:grit_lib::worktree::prune_linked_worktrees) accepts [`PruneWorktreesOptions`](rustdoc:grit_lib::worktree::PruneWorktreesOptions) and returns [`PrunedWorktreeEntry`](rustdoc:grit_lib::worktree::PrunedWorktreeEntry) rows with a reason string.

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

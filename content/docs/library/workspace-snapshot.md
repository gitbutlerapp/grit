# Workspace snapshot

Front ends that record an operation log need to undo more than a commit oid: conflict stages, worktree bytes (including conflict markers), named refs, `HEAD`, and merge/rebase sentinels must round-trip together.

[`Repository::workspace_snapshot`](rustdoc:grit_lib::repo::Repository::workspace_snapshot) exposes capture and restore. Stash [`create`](rustdoc:grit_lib::repo_handles::RepositoryStash::create) still refuses an unmerged index; use a workspace snapshot instead.

## Capture

[`WorkspaceCaptureOptions`](rustdoc:grit_lib::porcelain::workspace_snapshot::WorkspaceCaptureOptions) lists refs to store as [`RawRef`](rustdoc:grit_lib::refs::store::RawRef) values and whether untracked paths belong in the worktree tree. [`capture_workspace_snapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::capture_workspace_snapshot) writes:

- [`HeadState`](rustdoc:grit_lib::state::HeadState)
- index file bytes (all conflict stages preserved)
- a worktree tree built from on-disk file contents
- operation sentinel files under `.git` (`MERGE_*`, `sequencer/`, `rebase-merge/`, …)

## Restore

[`restore_workspace_snapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::restore_workspace_snapshot) updates refs and symbolic `HEAD` in one transaction, writes the index bytes, checks out the worktree tree, then restores sentinels.

Before refs move, restore builds a tree from paths in the pre-restore index (all stages) and the snapshot worktree tree, hashing only files that exist on disk. Checkout diffs that tree against the snapshot worktree tree so undoing a branch switch or uncommitted deletions restores or removes paths even when they matched `HEAD` at capture. Untracked files that appear after capture and are not in the index or snapshot tree are left alone when `include_untracked` was false at capture.

## Clearing operation state

[`state::clear_in_progress`](rustdoc:grit_lib::state::clear_in_progress) removes merge/rebase/cherry-pick sentinels without moving `HEAD` or the worktree (unlike [`abort_branch_merge`](rustdoc:grit_lib::porcelain::merge::abort_branch_merge)).

## Stash apply helper

[`StashApplyOptions::overwrite_identical_untracked`](rustdoc:grit_lib::porcelain::stash::StashApplyOptions::overwrite_identical_untracked) lets apply succeed when an untracked file on disk already matches the stashed untracked parent (for example after `reset --hard` left the file in place). The CLI default stays `false`.

[`StashPushOptions::new`](rustdoc:grit_lib::porcelain::stash::StashPushOptions::new) builds push options with only a committer identity required.

## Persistence and object lifetime

Front ends that run one process per command must store a captured snapshot and reload it in a later process. With the `serde-models` feature, [`WorkspaceSnapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::WorkspaceSnapshot) implements `serde::Serialize` and `serde::Deserialize`. Serialize with a **binary** format (for example Postcard or Bincode): `index_bytes` and the values in `operation_state` are raw bytes, not JSON-friendly text. `index_mtime` is encoded as optional Unix seconds and nanoseconds.

[`capture_workspace_snapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::capture_workspace_snapshot) is non-mutating and does **not** keep objects alive. The worktree tree and any conflict-marker or untracked blobs it references are loose objects with no ref until you pin them. After `git gc --prune=now` or grit prune they become unreachable and restore will fail.

Call [`pin_workspace_snapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::pin_workspace_snapshot) (or [`RepositoryWorkspaceSnapshot::pin`](rustdoc:grit_lib::repo_handles::RepositoryWorkspaceSnapshot::pin)) with a caller-chosen id after capture. That writes `refs/grit/workspace-snapshots/<id>` pointing directly at [`WorkspaceSnapshot::worktree_tree`](rustdoc:grit_lib::porcelain::workspace_snapshot::WorkspaceSnapshot::worktree_tree). The ref is outside `refs/heads/`, so a tree oid is valid and both Git and grit garbage collection retain the tree and reachable blobs. Keep the pin until the undo entry is discarded, then [`unpin_workspace_snapshot`](rustdoc:grit_lib::porcelain::workspace_snapshot::unpin_workspace_snapshot) so gc can reclaim the objects.

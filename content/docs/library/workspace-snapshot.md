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

## Clearing operation state

[`state::clear_in_progress`](rustdoc:grit_lib::state::clear_in_progress) removes merge/rebase/cherry-pick sentinels without moving `HEAD` or the worktree (unlike [`abort_branch_merge`](rustdoc:grit_lib::porcelain::merge::abort_branch_merge)).

## Stash apply helper

[`StashApplyOptions::overwrite_identical_untracked`](rustdoc:grit_lib::porcelain::stash::StashApplyOptions::overwrite_identical_untracked) lets apply succeed when an untracked file on disk already matches the stashed untracked parent (for example after `reset --hard` left the file in place). The CLI default stays `false`.

[`StashPushOptions::new`](rustdoc:grit_lib::porcelain::stash::StashPushOptions::new) builds push options with only a committer identity required.

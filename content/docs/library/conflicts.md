---
title: Conflicts during replay
summary: Multi-commit cherry-pick and revert sequences, on-disk sequencer state, and continue/abort.
---

Pick and revert ranges use [`porcelain::replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence) instead of single-commit [`replay_commit`](rustdoc:grit_lib::porcelain::replay::replay_commit). On conflict, grit writes Git-compatible `CHERRY_PICK_HEAD` or `REVERT_HEAD`, `MERGE_MSG`, `ORIG_HEAD`, and `.git/sequencer/` (todo, head, abort-safety, opts) so `git cherry-pick --continue` and `grit pick --continue` can interoperate.

[`ReplayOptions`](rustdoc:grit_lib::porcelain::replay_sequence::ReplayOptions) controls mainline parent selection for merge commits, `-x` / `record_origin`, empty-commit policy (`Stop`, `Drop`, `Keep`), merge favor, and conflict marker style. [`start_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::start_replay_sequence) returns [`SequenceOutcome`](rustdoc:grit_lib::porcelain::replay_sequence::SequenceOutcome) — `Completed`, `Conflicts`, or `Empty`. Every commit created so far is a [`ReplayCreated`](rustdoc:grit_lib::porcelain::replay_sequence::ReplayCreated) carrying its source, new oid, stored change id, and subject (also for steps committed before an earlier stop), and a stop reports the subject of the commit it stopped on, so callers can render results without re-reading commits. Resolve conflicts in the worktree, stage paths, then call [`continue_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::continue_replay_sequence). [`skip_replay_step`](rustdoc:grit_lib::porcelain::replay_sequence::skip_replay_step) and [`abort_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::abort_replay_sequence) match Git's skip/abort semantics. Abort rewinds to `sequencer/head` when grit wrote a sequencer directory, otherwise to the current branch tip (for example a single pick or revert started by Git); a leftover `ORIG_HEAD` from an earlier reset or merge is not the restore target. Abort clears replay state without moving the branch when `HEAD` no longer matches `sequencer/abort-safety`.

Conflict markers and unmerged index stages reuse the same tree-merge path as [`porcelain::merge`](rustdoc:grit_lib::porcelain::merge) (`apply_conflict_merge_worktree`). [`status`](rustdoc:grit_lib::porcelain::status::status) surfaces cherry-pick/revert operation guidance via [`state::status_operation`](rustdoc:grit_lib::state::status_operation). On the returned model, [`StatusModel::conflict_groups`](rustdoc:grit_lib::porcelain::status::StatusModel::conflict_groups) groups unmerged paths by [`ConflictKind`](rustdoc:grit_lib::operation::ConflictKind), and [`StatusModel::guidance`](rustdoc:grit_lib::porcelain::status::StatusModel::guidance) picks the next step `grit status` suggests: the paused operation's runnable steps, the conflict and merge fallbacks, or everyday [`StatusAction`](rustdoc:grit_lib::porcelain::status::StatusAction)s (discard, stage, commit, pull when strictly behind).

## Sequencer and merge errors ([`MergeError`](rustdoc:grit_lib::porcelain::MergeError))

[`start_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::start_replay_sequence), [`continue_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::continue_replay_sequence), skip, and abort propagate the same [`MergeError`](rustdoc:grit_lib::porcelain::MergeError) variants as merge (wrapped in [`Error::Merge`](rustdoc:grit_lib::error::Error::Merge)). Examples: [`ReplayContinueUnmerged`](rustdoc:grit_lib::porcelain::MergeError::ReplayContinueUnmerged) when continue is attempted with unmerged index entries; [`ReplayContinueDirtyIndex`](rustdoc:grit_lib::porcelain::MergeError::ReplayContinueDirtyIndex) when the index differs from `HEAD` after the step was already committed; [`NoReplayInProgress`](rustdoc:grit_lib::porcelain::MergeError::NoReplayInProgress) for continue/skip/abort with no sequence; [`OperationInProgress`](rustdoc:grit_lib::porcelain::MergeError::OperationInProgress) when a merge (or other paused operation) blocks starting a new pick/revert. Merge commits without [`ReplayOptions::mainline`](rustdoc:grit_lib::porcelain::replay_sequence::ReplayOptions::mainline) return [`MergeCommit`](rustdoc:grit_lib::porcelain::MergeError::MergeCommit).

## One conflict model

Every API that reports conflicts uses [`operation::Conflict`](rustdoc:grit_lib::operation::Conflict) (a path and a [`ConflictKind`](rustdoc:grit_lib::operation::ConflictKind)): [`StatusModel::conflict_details`](rustdoc:grit_lib::porcelain::status::StatusModel::conflict_details), merge and pull outcomes, [`SequenceOutcome::Conflicts`](rustdoc:grit_lib::porcelain::replay_sequence::SequenceOutcome::Conflicts), a stopped rebase's [`RebaseStop`](rustdoc:grit_lib::revwalk::history::RebaseStop), rebase predictions and [`HistoryError::Conflict`](rustdoc:grit_lib::revwalk::history::HistoryError::Conflict), and [`StashApplyOutcome`](rustdoc:grit_lib::stash::StashApplyOutcome). The kind is read from which unmerged index stages a path has ([`conflict_kind_from_stagemask`](rustdoc:grit_lib::operation::conflict_kind_from_stagemask), the same classification as Git's porcelain v2 `u` lines), so an operation's conflicts equal what `status` reports right after it stops, and a UI needs one renderer. Only a tree-merge result ([`conflicts_from_tree_merge`](rustdoc:grit_lib::operation::conflicts_from_tree_merge)) can add [`ConflictKind::RenameRename`](rustdoc:grit_lib::operation::ConflictKind::RenameRename) and the directory rename kinds ([`DirectoryRenameMoved`](rustdoc:grit_lib::operation::ConflictKind::DirectoryRenameMoved), [`DirectoryRenameSplit`](rustdoc:grit_lib::operation::ConflictKind::DirectoryRenameSplit), [`DirectoryRenameBlocked`](rustdoc:grit_lib::operation::ConflictKind::DirectoryRenameBlocked)), which need information the index does not keep; [`conflicts_from_index`](rustdoc:grit_lib::operation::conflicts_from_index) classifies an index alone. [`ConflictKind::as_str`](rustdoc:grit_lib::operation::ConflictKind::as_str) gives the snake_case name `grit` prints in `--json`, and with the `serde-models` feature a `Conflict` serializes to the same `{"path", "kind"}` object.

```rust
use grit_lib::operation::ConflictKind;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};

let model = status(&repo, &StatusOptions::default(), &mut NullProgress, &NeverCancel)?;
for conflict in &model.conflict_details {
    if conflict.kind == ConflictKind::DeletedByUs {
        println!("{}: deleted here, changed on the incoming side", conflict.path);
    }
}
```

## Example

<!-- include: grit-examples/src/bin/guide_replay_conflicts.rs -->

The integration test `replay_sequence_git_compat` cross-checks trees, messages, and on-disk state against system `git` after `git fsck --strict`.

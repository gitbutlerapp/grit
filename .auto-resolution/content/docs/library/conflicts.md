---
title: Conflicts during replay
summary: Multi-commit cherry-pick and revert sequences, on-disk sequencer state, and continue/abort.
---

Pick and revert ranges use [`porcelain::replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence) instead of single-commit [`replay_commit`](rustdoc:grit_lib::porcelain::replay::replay_commit). On conflict, grit writes Git-compatible `CHERRY_PICK_HEAD` or `REVERT_HEAD`, `MERGE_MSG`, `ORIG_HEAD`, and `.git/sequencer/` (todo, head, abort-safety, opts) so `git cherry-pick --continue` and `grit pick --continue` can interoperate.

[`ReplayOptions`](rustdoc:grit_lib::porcelain::replay_sequence::ReplayOptions) controls mainline parent selection for merge commits, `-x` / `record_origin`, empty-commit policy (`Stop`, `Drop`, `Keep`), merge favor, and conflict marker style. [`start_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::start_replay_sequence) returns [`SequenceOutcome`](rustdoc:grit_lib::porcelain::replay_sequence::SequenceOutcome) — `Completed`, `Conflicts`, or `Empty`. Resolve conflicts in the worktree, stage paths, then call [`continue_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::continue_replay_sequence). [`skip_replay_step`](rustdoc:grit_lib::porcelain::replay_sequence::skip_replay_step) and [`abort_replay_sequence`](rustdoc:grit_lib::porcelain::replay_sequence::abort_replay_sequence) match Git's skip/abort semantics (abort refuses when `HEAD` moved past `sequencer/abort-safety`).

Conflict markers and unmerged index stages reuse the same tree-merge path as [`porcelain::merge`](rustdoc:grit_lib::porcelain::merge) (`apply_conflict_merge_worktree`). [`status`](rustdoc:grit_lib::porcelain::status::status) surfaces cherry-pick/revert operation guidance via [`state::status_operation`](rustdoc:grit_lib::state::status_operation).

## Example

<!-- include: grit-examples/src/bin/guide_replay_conflicts.rs -->

The integration test `replay_sequence_git_compat` cross-checks trees, messages, and on-disk state against system `git` after `git fsck --strict`.

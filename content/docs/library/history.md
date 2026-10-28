---
title: History rewrite
summary: Plan in-memory history edits, then apply ref and worktree updates atomically.
---

The [`history`](rustdoc:grit_lib::revwalk::history) module supports stateless history editing: build a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan) in memory, inspect or dry-run it, then [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) to move refs and align the worktree.

## Model

1. **Plan** — [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) walks affected commits, writes replacement objects into the ODB, and records ref movements in a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Refs, the index, and the worktree are untouched during planning.
2. **Apply** — [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) writes a crash journal under `.git/grit/rewrite-apply/` when a worktree checkout is required, runs a compare-and-swap ref batch, then (in non-bare repos) checks out from the old `HEAD` tree to the new one while preserving unrelated staged and unstaged changes. On success the journal is removed. If the process dies after refs move but before checkout finishes, [`recover_interrupted_rewrite_apply`](rustdoc:grit_lib::revwalk::history::recover_interrupted_rewrite_apply) (also run from [`status`](rustdoc:grit_lib::porcelain::status::status) and before [`create_commit`](rustdoc:grit_lib::porcelain::commit::create_commit)) finishes the checkout when refs already match the plan, or rolls refs and the index/worktree back from the journal. Recovery holds `index.lock` for its whole run: when grit recorded a pid sidecar for the lock (the default; see [`grit_lockfile_pid_enabled`](rustdoc:grit_lib::lockfile::grit_lockfile_pid_enabled)) and that process has exited, [`LockHolder::remove_if_stale`](rustdoc:grit_lib::lockfile::LockHolder::remove_if_stale) removes the lock first; any other existing lock returns [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld) without touching refs. To finish, it first restores the journal's index and the transition paths it captured (a kill mid-checkout can leave files half written), then checks out the new tree again, so the partial checkout is not mistaken for local edits. Only if that checkout still fails are refs rolled back, and the call returns [`RewriteApplyRecovery::RolledBack`](rustdoc:grit_lib::revwalk::history::RewriteApplyRecovery::RolledBack) rather than the checkout error. A journal directory without its `meta` file (killed while the snapshots were written, before any ref moved) is removed. [`status`](rustdoc:grit_lib::porcelain::status::status) reports what happened in [`StatusModel::interrupted_rewrite`](rustdoc:grit_lib::porcelain::status::StatusModel::interrupted_rewrite), with [`InterruptedRewrite::Pending`](rustdoc:grit_lib::porcelain::status::InterruptedRewrite::Pending) when another process holds the index lock. While the journal remains, [`HistoryError::RewriteApplyIncomplete`](rustdoc:grit_lib::revwalk::history::HistoryError::RewriteApplyIncomplete) blocks new commits; `grit rebase --abort` calls [`abort_interrupted_rewrite_apply`](rustdoc:grit_lib::revwalk::history::abort_interrupted_rewrite_apply) to roll back explicitly.

[`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) describes per-commit rewrites, drops, and insert-before commits. [`RewriteOptions`](rustdoc:grit_lib::revwalk::history::RewriteOptions) controls scope, empty-commit policy, the committer [`Signature`](rustdoc:grit_lib::ident::Signature) used for new objects (usually `repo.identity(IdentRole::Committer, now)`), and whether replacement commits are signed ([`RewriteOptions::sign`](rustdoc:grit_lib::revwalk::history::RewriteOptions::sign) — set `false` for dry-run planning so signers are not invoked).

When a rewrite replays descendant **merge** commits, [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) remaps every parent and recomputes the merge tree (first-parent transplant when side branches are unchanged, otherwise a fresh merge of the mapped parents). Dropping or reordering a merge commit itself is still rejected ([`HistoryError::MergeCommitInReplay`](rustdoc:grit_lib::revwalk::history::HistoryError::MergeCommitInReplay)).

For a single message change, [`reword`](rustdoc:grit_lib::revwalk::history::reword) builds the [`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) and runs [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) after [`normalize_rewrite_message`](rustdoc:grit_lib::revwalk::history::normalize_rewrite_message) (stripspace, trailing newline, reject empty).

To reorder one commit on the linear first-parent chain of `HEAD`, [`move_commit`](rustdoc:grit_lib::revwalk::history::move_commit) takes a [`Placement`](rustdoc:grit_lib::revwalk::history::Placement) (`Before` / `After` another commit), replays the affected segment in the new order, and returns a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Commits must sit on the same first-parent chain with no merge commits in the segment; conflicts and no-op moves surface as [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError) variants.

For rebasing a branch tip onto a new base with fork-point detection and bounded cherry patch-id lookup, see [Rebase planning](rebase.md) ([`plan_rebase`](rustdoc:grit_lib::revwalk::history::plan_rebase) / [`RebaseRequest`](rustdoc:grit_lib::revwalk::history::RebaseRequest)).

To assign staged hunks automatically, [`absorb`](rustdoc:grit_lib::revwalk::history::absorb) blame-maps each hunk to a candidate commit between [`AbsorbOptions::base`](rustdoc:grit_lib::revwalk::history::AbsorbOptions::base) and `HEAD`, verifies commutation with later commits, builds one combined [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), and returns [`AbsorbPlan`](rustdoc:grit_lib::revwalk::history::AbsorbPlan) with [`HunkAssignment`](rustdoc:grit_lib::revwalk::history::HunkAssignment) and [`UnassignedHunk`](rustdoc:grit_lib::revwalk::history::UnassignedHunk) detail. When `--base` is omitted, [`AbsorbPlan::excluded_refs`](rustdoc:grit_lib::revwalk::history::AbsorbPlan::excluded_refs) lists other local branch tips that bound the stack (another ref at the checked-out tip is not treated as a boundary). [`UnassignedReason::HiddenByRef`](rustdoc:grit_lib::revwalk::history::UnassignedReason::HiddenByRef) with [`UnassignedHunk::detail`](rustdoc:grit_lib::revwalk::history::UnassignedHunk::detail) explains when a hunk’s introducing commit sits outside the stack because of one of those tips. Call [`apply_absorb_rewrite`](rustdoc:grit_lib::revwalk::history::apply_absorb_rewrite) so only assigned hunks are removed from the index.

## Apply options and result

[`ApplyOptions`](rustdoc:grit_lib::revwalk::history::ApplyOptions) supplies the reflog identity and message written for every ref update in the batch.

[`AppliedRewrite`](rustdoc:grit_lib::revwalk::history::AppliedRewrite) returns the ref updates that were committed and the commit OID `HEAD` resolves to afterward.

## Undo

When [`ApplyOptions::record_operation`](rustdoc:grit_lib::revwalk::history::ApplyOptions::record_operation) is set, the apply records its ref plan in `.git/grit/history-last`, and [`undo_last_rewrite`](rustdoc:grit_lib::revwalk::history::undo_last_rewrite) inverts it while every recorded ref is still at its post-rewrite tip. Undo writes the inverse as the new record, so calling it again redoes the rewrite.

[`apply_fixup_rewrite`](rustdoc:grit_lib::revwalk::history::apply_fixup_rewrite), [`apply_fixup_hunk_selection`](rustdoc:grit_lib::revwalk::history::apply_fixup_hunk_selection), and [`apply_absorb_rewrite`](rustdoc:grit_lib::revwalk::history::apply_absorb_rewrite) consume edits from the index (or selected unstaged hunks). A recorded apply also saves, in `.git/grit/history-last-work`, each consumed path's index entry and work tree file from before the command, plus a fingerprint of the same paths after it. Undo checks that those paths still match the fingerprint (otherwise [`LocalChangesWouldBeOverwritten`](rustdoc:grit_lib::revwalk::history::HistoryError::LocalChangesWouldBeOverwritten)), runs the inverse checkout, then puts the saved entries and files back. A restored entry takes the stat data of the file just written when that file cleans (through `core.autocrlf`, `eol`, `ident` and clean filters) to the restored blob, so neither Git nor grit sees a phantom unstaged change; when the file holds a real unstaged edit, the entry's stat cache stays unset so both rehash it. [`UndoOutcome::restored_paths`](rustdoc:grit_lib::revwalk::history::UndoOutcome::restored_paths) lists them. A record written without saved state still moves refs, and [`UndoOutcome::unrestored_edits`](rustdoc:grit_lib::revwalk::history::UndoOutcome::unrestored_edits) names the commit that holds the consumed edits.

## Inspecting a plan

[`RewritePlan::ref_update_lines`](rustdoc:grit_lib::revwalk::history::RewritePlan::ref_update_lines) returns `update <ref> <new> <old>` lines suitable for `git update-ref --stdin`, so callers can preview ref motion without applying.

[`RewritePlan::replacement`](rustdoc:grit_lib::revwalk::history::RewritePlan::replacement) maps one original commit to its rewritten OID (`None` when dropped), and [`RewritePlan::head_change`](rustdoc:grit_lib::revwalk::history::RewritePlan::head_change) reports `HEAD`'s commit before and after the rewrite, or `None` when `HEAD` does not move. `grit history` builds its `--json` `head` field from it.

## Change ids

History planning preserves [`change-id`](rustdoc:grit_lib::change_id::change_id) and legacy `gitbutler-change-id` headers on rewritten commits (signatures are stripped). When [`ChangeIdPolicy`](rustdoc:grit_lib::change_id::ChangeIdPolicy) is enabled and the source commit has no stored id, the planner writes the Jujutsu-derived id for the original commit oid into the replacement object. Commits inserted during a rewrite (for example the parent created by [`split`](rustdoc:grit_lib::revwalk::history::split)) receive a new random id via [`ChangeIdPolicy::id_for_new_commit`](rustdoc:grit_lib::change_id::ChangeIdPolicy::id_for_new_commit). With `grit.changeIds=false`, existing headers are still copied but no derived or random ids are added. See [Change ids](change-ids.md) for header format and config.

## Errors

[`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError) covers planning failures (conflicts, out-of-scope commits, branch checked out elsewhere) and apply failures:

- [`LocalChangesWouldBeOverwritten`](rustdoc:grit_lib::revwalk::history::HistoryError::LocalChangesWouldBeOverwritten) — staged, unstaged, or untracked paths would be clobbered by the planned tree checkout.
- [`RefCasMismatch`](rustdoc:grit_lib::revwalk::history::HistoryError::RefCasMismatch) — a ref tip changed after planning; nothing is written.
- [`HeadMovedSincePlan`](rustdoc:grit_lib::revwalk::history::HistoryError::HeadMovedSincePlan) — `HEAD` no longer points at the ref or commit recorded when the plan was built; nothing is written.
- [`RefApply`](rustdoc:grit_lib::revwalk::history::HistoryError::RefApply) / [`WorktreeApply`](rustdoc:grit_lib::revwalk::history::HistoryError::WorktreeApply) — ref batch or post-ref checkout failed (apply rolls back ref updates when checkout fails).
- [`RewriteApplyIncomplete`](rustdoc:grit_lib::revwalk::history::HistoryError::RewriteApplyIncomplete) — refs moved but the worktree checkout did not finish; run recovery or `grit rebase --abort` before committing.
- [`ApplyRollbackIncomplete`](rustdoc:grit_lib::revwalk::history::HistoryError::ApplyRollbackIncomplete) — checkout failed after refs moved and ref rollback or worktree restore also failed.
- [`EmptyMessage`](rustdoc:grit_lib::revwalk::history::HistoryError::EmptyMessage) — reword message cleanup left an empty body.
- [`Signing`](rustdoc:grit_lib::revwalk::history::HistoryError::Signing) — `commit.gpgsign` was enabled but signing a replacement commit failed; refs are not updated.

## Example

Reword one commit on the current branch, plan, and apply:

<!-- include: grit-examples/src/bin/history_reword.rs -->

```bash
cargo run --bin history_reword /path/to/repo <commit-ish> "New message"
```

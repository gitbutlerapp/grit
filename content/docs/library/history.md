---
title: History rewrite
summary: Plan in-memory history edits, then apply ref and worktree updates atomically.
---

The [`history`](rustdoc:grit_lib::revwalk::history) module supports stateless history editing: build a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan) in memory, inspect or dry-run it, then [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) to move refs and align the worktree.

## Model

1. **Plan** — [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) walks affected commits, writes replacement objects into the ODB, and records ref movements in a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Refs, the index, and the worktree are untouched during planning.
2. **Apply** — [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) runs a compare-and-swap ref batch, then (in non-bare repos) checks out from the old `HEAD` tree to the new one while preserving unrelated staged and unstaged changes.

[`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) describes per-commit rewrites, drops, and insert-before commits. [`RewriteOptions`](rustdoc:grit_lib::revwalk::history::RewriteOptions) controls scope, empty-commit policy, and the committer ident used for new objects.

When a rewrite replays descendant **merge** commits, [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) remaps every parent and recomputes the merge tree (first-parent transplant when side branches are unchanged, otherwise a fresh merge of the mapped parents). Dropping or reordering a merge commit itself is still rejected ([`HistoryError::MergeCommitInReplay`](rustdoc:grit_lib::revwalk::history::HistoryError::MergeCommitInReplay)).

For a single message change, [`reword`](rustdoc:grit_lib::revwalk::history::reword) builds the [`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) and runs [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) after [`normalize_rewrite_message`](rustdoc:grit_lib::revwalk::history::normalize_rewrite_message) (stripspace, trailing newline, reject empty).

To reorder one commit on the linear first-parent chain of `HEAD`, [`move_commit`](rustdoc:grit_lib::revwalk::history::move_commit) takes a [`Placement`](rustdoc:grit_lib::revwalk::history::Placement) (`Before` / `After` another commit), replays the affected segment in the new order, and returns a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Commits must sit on the same first-parent chain with no merge commits in the segment; conflicts and no-op moves surface as [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError) variants.

To assign staged hunks automatically, [`absorb`](rustdoc:grit_lib::revwalk::history::absorb) blame-maps each hunk to a candidate commit between [`AbsorbOptions::base`](rustdoc:grit_lib::revwalk::history::AbsorbOptions::base) and `HEAD`, verifies commutation with later commits, builds one combined [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), and returns [`AbsorbPlan`](rustdoc:grit_lib::revwalk::history::AbsorbPlan) with [`HunkAssignment`](rustdoc:grit_lib::revwalk::history::HunkAssignment) and [`UnassignedHunk`](rustdoc:grit_lib::revwalk::history::UnassignedHunk) detail. Call [`apply_absorb_rewrite`](rustdoc:grit_lib::revwalk::history::apply_absorb_rewrite) so only assigned hunks are removed from the index.

## Apply options and result

[`ApplyOptions`](rustdoc:grit_lib::revwalk::history::ApplyOptions) supplies the reflog identity and message written for every ref update in the batch.

[`AppliedRewrite`](rustdoc:grit_lib::revwalk::history::AppliedRewrite) returns the ref updates that were committed and the commit OID `HEAD` resolves to afterward.

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
- [`ApplyRollbackIncomplete`](rustdoc:grit_lib::revwalk::history::HistoryError::ApplyRollbackIncomplete) — checkout failed after refs moved and ref rollback or worktree restore also failed.
- [`EmptyMessage`](rustdoc:grit_lib::revwalk::history::HistoryError::EmptyMessage) — reword message cleanup left an empty body.

## Example

Reword one commit on the current branch, plan, and apply:

<!-- include: grit-examples/src/bin/history_reword.rs -->

```bash
cargo run --bin history_reword /path/to/repo <commit-ish> "New message"
```

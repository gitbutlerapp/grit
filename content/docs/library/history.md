---
title: History rewrite
summary: Plan in-memory history edits, then apply ref and worktree updates atomically.
---

The [`history`](rustdoc:grit_lib::revwalk::history) module supports stateless history editing: build a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan) in memory, inspect or dry-run it, then [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) to move refs and align the worktree.

## Model

1. **Plan** — [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) walks affected commits, writes replacement objects into the ODB, and records ref movements in a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Refs, the index, and the worktree are untouched during planning.
2. **Apply** — [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) runs a compare-and-swap ref batch, then (in non-bare repos) checks out from the old `HEAD` tree to the new one while preserving unrelated staged and unstaged changes.

[`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) describes per-commit rewrites, drops, and insert-before commits. [`RewriteOptions`](rustdoc:grit_lib::revwalk::history::RewriteOptions) controls scope, empty-commit policy, and the committer ident used for new objects.

For a single message change, [`reword`](rustdoc:grit_lib::revwalk::history::reword) builds the [`CommitEdit`](rustdoc:grit_lib::revwalk::history::CommitEdit) and runs [`plan_rewrite`](rustdoc:grit_lib::revwalk::history::plan_rewrite) after [`normalize_rewrite_message`](rustdoc:grit_lib::revwalk::history::normalize_rewrite_message) (stripspace, trailing newline, reject empty).

To reorder one commit on the linear first-parent chain of `HEAD`, [`move_commit`](rustdoc:grit_lib::revwalk::history::move_commit) takes a [`Placement`](rustdoc:grit_lib::revwalk::history::Placement) (`Before` / `After` another commit), replays the affected segment in the new order, and returns a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan). Commits must sit on the same first-parent chain with no merge commits in the segment; conflicts and no-op moves surface as [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError) variants.

## Apply options and result

[`ApplyOptions`](rustdoc:grit_lib::revwalk::history::ApplyOptions) supplies the reflog identity and message written for every ref update in the batch.

[`AppliedRewrite`](rustdoc:grit_lib::revwalk::history::AppliedRewrite) returns the ref updates that were committed and the commit OID `HEAD` resolves to afterward.

## Dry-run ref lines

[`RewritePlan::ref_update_lines`](rustdoc:grit_lib::revwalk::history::RewritePlan::ref_update_lines) returns `update <ref> <new> <old>` lines suitable for `git update-ref --stdin`, so callers can preview ref motion without applying.

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

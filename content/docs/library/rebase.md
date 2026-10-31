---
title: Rebase planning
summary: Plan a fork-point rebase with bounded cherry patch-id detection before applying ref updates.
---

[`plan_rebase`](rustdoc:grit_lib::revwalk::history::plan_rebase) builds a [`RebasePlan`](rustdoc:grit_lib::revwalk::history::RebasePlan) for `git rebase [--onto <base>] <upstream>` semantics on a single branch tip. [`plan_rebase_check`](rustdoc:grit_lib::revwalk::history::plan_rebase_check) returns [`RebaseCheckPlan`](rustdoc:grit_lib::revwalk::history::RebaseCheckPlan) (`UpToDate`, `Clean { commits }`, or `Conflict { commit, paths }`) without updating refs, the index, or the worktree.

Set [`RebaseOntoOptions::favor`](rustdoc:grit_lib::revwalk::history::RebaseOntoOptions::favor) to [`MergeFavor::Ours`](rustdoc:grit_lib::diff::merge_file::MergeFavor::Ours) or [`MergeFavor::Theirs`](rustdoc:grit_lib::diff::merge_file::MergeFavor::Theirs) to match `git rebase -X`. During replay merges, **ours** is the onto tree and **theirs** is the commit being replayed (Git’s rebase convention, not branch-merge “ours”). It computes the fork with [`merge_bases_first_vs_rest`](rustdoc:grit_lib::revwalk::merge_base::merge_bases_first_vs_rest), replays `fork..branch_tip` onto `onto.unwrap_or(upstream)`, and returns either [`RebasePlan::UpToDate`](rustdoc:grit_lib::revwalk::history::RebasePlan::UpToDate), a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), or [`RebasePlan::Conflict`](rustdoc:grit_lib::revwalk::history::RebasePlan::Conflict) (conflicts are values, not [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError), so callers can report paths without treating them as I/O failures).

Cherry detection (`skip_applied` on [`RebaseOntoOptions`](rustdoc:grit_lib::revwalk::history::RebaseOntoOptions)) walks only the onto side between fork and onto. Commits that do not touch any path changed in the replay range are skipped before hashing; a root commit on the onto side (such as an unrelated history merged with `--allow-unrelated-histories`) is diffed against the empty tree. Every candidate and range commit is compared using a full stable patch-id (matching `git patch-id --stable`). With no `--onto`, a linear branch that already extends `upstream` yields [`RebasePlan::UpToDate`](rustdoc:grit_lib::revwalk::history::RebasePlan::UpToDate), matching `git rebase <upstream>`. A skipped commit becomes a [`CommitEdit::Reuse`](rustdoc:grit_lib::revwalk::history::CommitEdit::Reuse) edit: its [`RewrittenCommit`](rustdoc:grit_lib::revwalk::history::RewrittenCommit) row names the upstream equivalent with `reused: true`, but descendants and refs continue from the edit's `parent` (the skipped commit's own replayed first parent, or the new base when it is a root of the range), so nothing before or after the skip is lost and the branch never lands on an older upstream commit. [`rebase_onto`](rustdoc:grit_lib::revwalk::history::rebase_onto) keeps the same replay engine for callers (such as pull) that already supply the fork as their upstream boundary.

After planning, apply with [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) the same way as other history edits. See [History rewrite](history.md) for scope, empty-commit policy, and change-id behavior.

## Stop and resume

[`start_rebase`](rustdoc:grit_lib::revwalk::history::start_rebase) runs resumable planning. When replay finishes without conflicts, it applies the plan atomically and returns [`RebaseOutcome::Done`](rustdoc:grit_lib::revwalk::history::RebaseOutcome::Done). When a commit conflicts, grit writes Git-compatible `.git/rebase-merge/` files plus `rebase-merge/grit-plan` (mapping, replay order, and options), detaches `HEAD` at the replayed parent, and leaves the branch ref at its pre-rebase tip — matching `git rebase` stop behavior.

Resolve conflicts in the worktree, stage paths, then [`continue_rebase`](rustdoc:grit_lib::revwalk::history::continue_rebase) to record the resolved tree and replay remaining commits. [`skip_rebase`](rustdoc:grit_lib::revwalk::history::skip_rebase) maps the current commit to `HEAD` and replans the rest. [`abort_rebase`](rustdoc:grit_lib::revwalk::history::abort_rebase) hard-resets the branch to `orig-head` and removes session state, even when something else moved the branch during the stop; it returns a [`RebaseAbort`](rustdoc:grit_lib::revwalk::history::RebaseAbort) whose `overwritten` list names each such branch with the tip it replaced (still in the branch reflog as `rebase (abort)`). For an exec stop the reset is a compare-and-swap against the tip just read; a concurrent move returns [`RebaseSessionError::AbortRefMoved`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::AbortRefMoved) and leaves the rebase in place to retry. [`continue_rebase`](rustdoc:grit_lib::revwalk::history::continue_rebase) returns [`RebaseSessionError::UnmergedPaths`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::UnmergedPaths) while the index still has conflict stages. If `rebase-merge/grit-plan` records something other than a hex object id where one belongs, `continue_rebase`, `skip_rebase` and `abort_rebase` return [`RebaseSessionError::InvalidStateOid`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::InvalidStateOid) with the offending text and change nothing.

On completion, [`RebaseOutcome::Done`](rustdoc:grit_lib::revwalk::history::RebaseOutcome::Done) exposes `head_change: Option<(ObjectId, ObjectId)>`. It compares `HEAD` at the start of that `start_rebase`, `continue_rebase`, or `skip_rebase` call with `applied.head` after completion and branch reattachment. For resume calls, the old OID is the stopped, detached `HEAD`, which can differ from the original branch tip in the plan's ref updates. `None` means the OID did not change, including a final skip that only reattaches `HEAD`. The plan retains its detached-HEAD metadata for apply validation; use `head_change` for the completed call's transition rather than deriving it from the plan. When [`ApplyOptions::record_operation`](rustdoc:grit_lib::revwalk::history::ApplyOptions::record_operation) is set, a finished `continue_rebase` or `skip_rebase` overwrites `.git/grit/history-last` after reattaching `HEAD` to the branch, using the pre-rebase branch tip and final tip (not the detached stop commit), so [`undo_last_rewrite`](rustdoc:grit_lib::revwalk::history::undo_last_rewrite) matches a one-shot rebase. The CLI exposes this same pair as `head: {old, new}` in completed JSON and as the HEAD row in Markdown.

## Exec verification and recovery

[`verify_rebase_with_exec`](rustdoc:grit_lib::revwalk::history::verify_rebase_with_exec)
checks out each replacement and runs the injected command runner. It takes the
plan by `&mut` with the plan's `RewriteOptions`: after a successful invocation, a
detached `HEAD` that amends the replay commit (same first parent) or adds commits on
top of it is adopted into the plan, later commits are replayed onto it with those
options, and ref updates and the `HEAD` effect follow. Any other `HEAD` move returns
[`HistoryError::ExecHeadMoved`](rustdoc:grit_lib::revwalk::history::HistoryError::ExecHeadMoved)
after a rollback. After every successful invocation it also reloads the index and
checks staged and unstaged tracked changes against the resulting `HEAD` tree,
including racy file timestamps. Clean commands restore the original HEAD, index, and
tracked files before the caller applies the updated plan atomically.

```rust
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::revwalk::history::{apply_rewrite, verify_rebase_with_exec, ExecSpec};

// `plan` comes from `plan_rebase(&repo, &request, &options)`.
let spec = ExecSpec { commands: vec!["cargo fmt && git commit -qa --amend --no-edit".into()] };
let report = verify_rebase_with_exec(&repo, &mut plan, &spec, &options.rewrite, runner.as_ref())?;
apply_rewrite(&repo, &plan, &apply, &mut NullProgress, &NeverCancel)?;
```

When replaying a later commit onto an amended one conflicts,
[`HistoryError::ExecConflict`](rustdoc:grit_lib::revwalk::history::HistoryError::ExecConflict)
leaves the conflict checked out on top of the rewritten commit and saves recovery
state: stage a resolution and call `continue_rebase` (commands then run at the
resolved commit), or `skip_rebase` to drop the commit (its commands still run at the
current `HEAD`, as in Git's todo list), or `abort_rebase`. The recovery record keeps
the `RewriteOptions` the exec run started with, and resuming uses those rather than
the options passed to `continue_rebase` / `skip_rebase`. If the record cannot be
written, the run is rolled back instead of leaving a detached conflict behind.

[`HistoryError::ExecDirty`](rustdoc:grit_lib::revwalk::history::HistoryError::ExecDirty)
carries the `HEAD` the command left (the replay commit or its amended replacement),
the command, changed paths, and captured run records. The
edited index and worktree are preserved at that detached HEAD. Original
branch refs remain unchanged. Do **not** apply the rewrite after this error.
The library saves the original HEAD and index, plan, and next command position in
`rebase-merge/grit-exec`; it distinguishes this stop from an unresolved pick.

Save edits with a stash, then call `continue_rebase` to run the next command and
remaining commits. `skip_rebase` skips remaining commands at the current commit
without dropping that commit. Both reject a dirty index/worktree without changing
it. A moved replay HEAD returns
[`RebaseSessionError::ExecHeadChanged`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::ExecHeadChanged):
save any new commits on a branch, abort, and restart. When the rebased branch, or a
ref the saved plan updates, was moved while stopped, both return
[`RebaseSessionError::StaleExecPlan`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::StaleExecPlan)
with the ref name and the expected and current tips, before checking the worktree and
without changing anything. `abort_rebase` explicitly
restores the saved HEAD, index, and tracked files, discarding unsaved exec edits.
Clean recovery returns `RebaseOutcome::ExecDone` with an `exec` report containing the
invocations captured during that call, for the embedding caller to render.
Nonzero exec exits keep their existing `ExecFailed` rollback behavior.

## Clean exec benchmark

Measured on Linux x86_64 with system Git 2.55.0 and Rust's `release` profile.
Each sample rebases a fresh copy of a repository with 200 or 2,000 tracked files
and 40 feature commits onto a one-commit upstream advance. Commands are `git rebase main
--exec true` and `grit rebase main --exec true --json`; output is captured. The table
shows medians of seven runs, measured together with `/usr/bin/time -v` after builds
and tests were idle.

| Tracked files | Implementation | Wall time | Peak RSS (KiB) |
| ---: | --- | ---: | ---: |
| 200 | System Git 2.55.0 | 220 ms | 6,924 |
| 200 | Grit before this fix (`34c10122c`) | 180 ms | 21,888 |
| 200 | Grit with dirty-exec detection | 280 ms | 22,316 |
| 2,000 | System Git 2.55.0 | 540 ms | 12,776 |
| 2,000 | Grit before this fix (`34c10122c`) | 1,780 ms | 32,420 |
| 2,000 | Grit with dirty-exec detection | 2,620 ms | 32,776 |

Checking the index and tracked worktree after every successful command adds
100 ms across 40 invocations (2.5 ms per command) and 428 KiB of peak RSS in this
200-file fixture. At 2,000 files, it adds 840 ms (21 ms per command) and 356 KiB.
This verification cost belongs to `--exec`; it closes the unchecked path that
previously discarded successful commands' edits. The wider fixture also exposes
an existing performance gap against Git that needs further work.

---
title: Rebase planning
summary: Plan a fork-point rebase with bounded cherry patch-id detection before applying ref updates.
---

[`plan_rebase`](rustdoc:grit_lib::revwalk::history::plan_rebase) builds a [`RebasePlan`](rustdoc:grit_lib::revwalk::history::RebasePlan) for `git rebase [--onto <base>] <upstream>` semantics on a single branch tip. [`plan_rebase_check`](rustdoc:grit_lib::revwalk::history::plan_rebase_check) returns [`RebaseCheckPlan`](rustdoc:grit_lib::revwalk::history::RebaseCheckPlan) (`UpToDate`, `Clean { commits }`, or `Conflict { commit, paths }`) without updating refs, the index, or the worktree.

Set [`RebaseOntoOptions::favor`](rustdoc:grit_lib::revwalk::history::RebaseOntoOptions::favor) to [`MergeFavor::Ours`](rustdoc:grit_lib::diff::merge_file::MergeFavor::Ours) or [`MergeFavor::Theirs`](rustdoc:grit_lib::diff::merge_file::MergeFavor::Theirs) to match `git rebase -X`. During replay merges, **ours** is the onto tree and **theirs** is the commit being replayed (Git’s rebase convention, not branch-merge “ours”). It computes the fork with [`merge_bases_first_vs_rest`](rustdoc:grit_lib::revwalk::merge_base::merge_bases_first_vs_rest), replays `fork..branch_tip` onto `onto.unwrap_or(upstream)`, and returns either [`RebasePlan::UpToDate`](rustdoc:grit_lib::revwalk::history::RebasePlan::UpToDate), a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), or [`RebasePlan::Conflict`](rustdoc:grit_lib::revwalk::history::RebasePlan::Conflict) (conflicts are values, not [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError), so callers can report paths without treating them as I/O failures).

Cherry detection (`skip_applied` on [`RebaseOntoOptions`](rustdoc:grit_lib::revwalk::history::RebaseOntoOptions)) walks only the onto side between fork and onto. Commits that do not touch any path changed in the replay range are skipped before hashing; every candidate and range commit is compared using a full stable patch-id (matching `git patch-id --stable`). With no `--onto`, a linear branch that already extends `upstream` yields [`RebasePlan::UpToDate`](rustdoc:grit_lib::revwalk::history::RebasePlan::UpToDate), matching `git rebase <upstream>`. [`rebase_onto`](rustdoc:grit_lib::revwalk::history::rebase_onto) keeps the same replay engine for callers (such as pull) that already supply the fork as their upstream boundary.

After planning, apply with [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) the same way as other history edits. See [History rewrite](history.md) for scope, empty-commit policy, and change-id behavior.

## Stop and resume

[`start_rebase`](rustdoc:grit_lib::revwalk::history::start_rebase) runs resumable planning. When replay finishes without conflicts, it applies the plan atomically and returns [`RebaseOutcome::Done`](rustdoc:grit_lib::revwalk::history::RebaseOutcome::Done). When a commit conflicts, grit writes Git-compatible `.git/rebase-merge/` files plus `rebase-merge/grit-plan` (mapping, replay order, and options), detaches `HEAD` at the replayed parent, and leaves the branch ref at its pre-rebase tip — matching `git rebase` stop behavior.

Resolve conflicts in the worktree, stage paths, then [`continue_rebase`](rustdoc:grit_lib::revwalk::history::continue_rebase) to record the resolved tree and replay remaining commits. [`skip_rebase`](rustdoc:grit_lib::revwalk::history::skip_rebase) maps the current commit to `HEAD` and replans the rest. [`abort_rebase`](rustdoc:grit_lib::revwalk::history::abort_rebase) hard-resets the branch to `orig-head` and removes session state. [`continue_rebase`](rustdoc:grit_lib::revwalk::history::continue_rebase) returns [`RebaseSessionError::UnmergedPaths`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::UnmergedPaths) while the index still has conflict stages.

## Exec verification and recovery

[`verify_rebase_with_exec`](rustdoc:grit_lib::revwalk::history::verify_rebase_with_exec)
checks out each replacement and runs the injected command runner. After every
successful invocation it reloads the index and checks staged and unstaged tracked
changes against the replay tree, including racy file timestamps. Clean commands
restore the original HEAD, index, and tracked files before the caller applies the
plan atomically.

[`HistoryError::ExecDirty`](rustdoc:grit_lib::revwalk::history::HistoryError::ExecDirty)
carries the replay commit, command, changed paths, and captured run records. The
edited index and worktree are preserved at the detached replay HEAD. Original
branch refs remain unchanged. Do **not** apply the rewrite after this error.
The library saves the original HEAD and index, plan, and next command position in
`rebase-merge/grit-exec`; it distinguishes this stop from an unresolved pick.

Save edits with a stash, then call `continue_rebase` to run the next command and
remaining commits. `skip_rebase` skips remaining commands at the current commit
without dropping that commit. Both reject a dirty index/worktree without changing
it. A moved replay HEAD returns
[`RebaseSessionError::ExecHeadChanged`](rustdoc:grit_lib::revwalk::history::RebaseSessionError::ExecHeadChanged):
save any new commits on a branch, abort, and restart. `abort_rebase` explicitly
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

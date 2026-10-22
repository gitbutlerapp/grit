---
title: Rebase planning
summary: Plan a fork-point rebase with bounded cherry patch-id detection before applying ref updates.
---

[`plan_rebase`](rustdoc:grit_lib::revwalk::history::plan_rebase) builds a [`RebasePlan`](rustdoc:grit_lib::revwalk::history::RebasePlan) for `git rebase [--onto <base>] <upstream>` semantics on a single branch tip. It computes the fork with [`merge_bases_first_vs_rest`](rustdoc:grit_lib::revwalk::merge_base::merge_bases_first_vs_rest), replays `fork..branch_tip` onto `onto.unwrap_or(upstream)`, and returns either [`RebasePlan::UpToDate`](rustdoc:grit_lib::revwalk::history::RebasePlan::UpToDate), a [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), or [`RebasePlan::Conflict`](rustdoc:grit_lib::revwalk::history::RebasePlan::Conflict) (conflicts are values, not [`HistoryError`](rustdoc:grit_lib::revwalk::history::HistoryError), so callers can report paths without treating them as I/O failures).

Cherry detection (`skip_applied` on [`RebaseOntoOptions`](rustdoc:grit_lib::revwalk::history::RebaseOntoOptions)) scans stable patch-ids only on the onto side between fork and onto, and only for commits whose touched paths intersect the replay range. [`rebase_onto`](rustdoc:grit_lib::revwalk::history::rebase_onto) keeps the same replay engine for callers (such as pull) that already supply the fork as their upstream boundary.

After planning, apply with [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) the same way as other history edits. See [History rewrite](history.md) for scope, empty-commit policy, and change-id behavior.

---
title: Merge
summary: Three-way tree merge and merge-base helpers in the diff and revwalk domains.
---

Merge **orchestration** (sequencer state, hooks, editors, messages) stays in `grit-cli`. The library exposes tree and index transforms embedders and porcelain reuse:

- [`revwalk::merge_base`](rustdoc:grit_lib::revwalk::merge_base) — common ancestors (`merge_bases_first_vs_rest`, ancestor checks).
- [`diff::merge_trees`](rustdoc:grit_lib::diff::merge_trees) — rename-aware three-way tree merge ([`merge_trees_three_way`](rustdoc:grit_lib::diff::merge_trees::merge_trees_three_way)), conflict-marker blobs, and [`TreeMergeOutput`](rustdoc:grit_lib::diff::merge_trees::TreeMergeOutput).
- [`porcelain::merge`](rustdoc:grit_lib::porcelain::merge) — fast-forward/octopus index composition, [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge) (in-memory conflict check via the ODB overlay), [`write_merge_tree`](rustdoc:grit_lib::porcelain::merge::write_merge_tree) (stored merge tree), and checkout helpers the CLI calls.

Pass explicit [`MergeFavor`](rustdoc:grit_lib::diff::merge_file::MergeFavor) and [`WhitespaceMergeOptions`](rustdoc:grit_lib::diff::merge_trees::WhitespaceMergeOptions) into tree merges; pass identity and timestamps into commit-writing porcelain.

## Predicting versus writing a merge

Both functions take two commits and [`MergePredictionOptions`](rustdoc:grit_lib::porcelain::merge::MergePredictionOptions), never touch refs, the index or the working tree, and work in bare repositories.

| Function | Objects written | Returns |
| --- | --- | --- |
| [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge) | None | Up to date, fast-forward, clean (with the tree id), conflicts, or unrelated |
| [`write_merge_tree`](rustdoc:grit_lib::porcelain::merge::write_merge_tree) | Every new tree and blob of the merge | [`WrittenMergeTree`](rustdoc:grit_lib::porcelain::merge::WrittenMergeTree): stored tree, conflicts, unmerged stages |

The tree id in [`MergePrediction::Clean`](rustdoc:grit_lib::porcelain::merge::MergePrediction) is the id `git merge-tree --write-tree` prints, but the tree is **not stored**: committing, diffing or reading it fails with `ObjectNotFound`. Use `predict_merge` to answer "would this merge cleanly?" cheaply (merge queues, `grit merge --check`). Use `write_merge_tree` when you need the result itself: it is the library form of `git merge-tree --write-tree`. A clean result can be committed straight away with `NewCommit::new(tree, ..).with_parents(vec![ours, theirs])`. On conflicts the stored tree holds conflict markers in the conflicted files, and [`WrittenMergeTree::stages`](rustdoc:grit_lib::porcelain::merge::WrittenMergeTree) lists the `mode`, `oid`, `stage` and `path` rows Git prints after the tree id. Set `ours_label` and `theirs_label` to choose the conflict-marker labels.

`write_merge_tree` always merges (the fast-forward option is ignored), uses the first merge base when there are several, and refuses commits with no common ancestor with `Error::UnrelatedHistories` unless `allow_unrelated_histories` is set.

## Example

The program resolves `ours` and `theirs` refs, stores the merge with `write_merge_tree`, prints the merged tree oid (compare with `git merge-tree --write-tree`) and, when the merge is clean, commits it with both parents:

<!-- include: grit-examples/src/bin/guide_merge.rs -->

The integration test builds non-conflicting `ours` / `theirs` branches, asserts the merged tree matches Git’s oracle, and runs `git fsck --strict` on the resulting merge commit.

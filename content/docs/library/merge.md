---
title: Merge
summary: Three-way tree merge and merge-base helpers in the diff and revwalk domains.
---

Merge **orchestration** (sequencer state, hooks, editors, messages) stays in `grit-cli`. The library exposes tree and index transforms embedders and porcelain reuse:

- [`revwalk::merge_base`](rustdoc:grit_lib::revwalk::merge_base) — common ancestors (`merge_bases_first_vs_rest`, ancestor checks).
- [`diff::merge_trees`](rustdoc:grit_lib::diff::merge_trees) — rename-aware three-way tree merge ([`merge_trees_three_way`](rustdoc:grit_lib::diff::merge_trees::merge_trees_three_way)), conflict-marker blobs, and [`TreeMergeOutput`](rustdoc:grit_lib::diff::merge_trees::TreeMergeOutput).
- [`porcelain::merge`](rustdoc:grit_lib::porcelain::merge) — fast-forward/octopus index composition, [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge) (in-memory conflict check via the ODB overlay), and checkout helpers the CLI calls.

Pass explicit [`MergeFavor`](rustdoc:grit_lib::diff::merge_file::MergeFavor) and [`WhitespaceMergeOptions`](rustdoc:grit_lib::diff::merge_trees::WhitespaceMergeOptions) into tree merges; pass identity and timestamps into commit-writing porcelain.

## Example

The program resolves `ours` and `theirs` refs, computes the merge base, runs `merge_trees_three_way`, and prints the merged tree oid (compare with `git merge-tree --write-tree`):

<!-- include: grit-examples/src/bin/guide_merge.rs -->

The integration test builds non-conflicting `ours` / `theirs` branches and asserts the merged tree matches Git’s oracle.

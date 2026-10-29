---
title: Merge
summary: Three-way tree merge and merge-base helpers in the diff and revwalk domains.
---

Merge **orchestration** (sequencer state, hooks, editors, messages) stays in `grit-cli`. The library exposes tree and index transforms embedders and porcelain reuse:

- [`revwalk::merge_base`](rustdoc:grit_lib::revwalk::merge_base) — common ancestors (`merge_bases_first_vs_rest`, ancestor checks).
- [`diff::merge_trees`](rustdoc:grit_lib::diff::merge_trees) — rename-aware three-way tree merge ([`merge_trees_three_way`](rustdoc:grit_lib::diff::merge_trees::merge_trees_three_way)), conflict-marker blobs, and [`TreeMergeOutput`](rustdoc:grit_lib::diff::merge_trees::TreeMergeOutput).
- [`porcelain::merge`](rustdoc:grit_lib::porcelain::merge) — fast-forward/octopus index composition, [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge) (in-memory conflict check via the ODB overlay), [`write_merge_tree`](rustdoc:grit_lib::porcelain::merge::write_merge_tree) (stored merge tree), and checkout helpers the CLI calls.

When two commits have several merge bases (criss-cross history), [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge), [`write_merge_tree`](rustdoc:grit_lib::porcelain::merge::write_merge_tree) and [`integrate_branch`](rustdoc:grit_lib::porcelain::merge::integrate_branch) merge the bases into one virtual base and merge against that, as Git's recursive and ort strategies do. The bases are taken oldest first, in the order [`merge_bases_all`](rustdoc:grit_lib::revwalk::merge_base::merge_bases_all) reports them (equal commit dates keep Git's order). Content conflicts between the bases stay in the virtual base as nested conflict markers. Binary, modify/delete and symlink conflicts keep the older version, and the caller's `MergeFavor` applies only to the final merge. A single merge base is a plain three-way merge. `merge_trees_three_way` itself always takes one explicit base tree.

Pass explicit [`MergeFavor`](rustdoc:grit_lib::diff::merge_file::MergeFavor) and [`WhitespaceMergeOptions`](rustdoc:grit_lib::diff::merge_trees::WhitespaceMergeOptions) into tree merges; pass identity and timestamps into commit-writing porcelain.

## Predicting versus writing a merge

Both functions take two commits and [`MergePredictionOptions`](rustdoc:grit_lib::porcelain::merge::MergePredictionOptions), never touch refs, the index or the working tree, and work in bare repositories.

| Function | Objects written | Returns |
| --- | --- | --- |
| [`predict_merge`](rustdoc:grit_lib::porcelain::merge::predict_merge) | None | Up to date, fast-forward, clean (with the tree id), conflicts, or unrelated |
| [`write_merge_tree`](rustdoc:grit_lib::porcelain::merge::write_merge_tree) | Every new tree and blob of the merge | [`WrittenMergeTree`](rustdoc:grit_lib::porcelain::merge::WrittenMergeTree): stored tree, conflicts, unmerged stages |

The tree id in [`MergePrediction::Clean`](rustdoc:grit_lib::porcelain::merge::MergePrediction) is the id `git merge-tree --write-tree` prints, but the tree is **not stored**: committing, diffing or reading it fails with `ObjectNotFound`. Use `predict_merge` to answer "would this merge cleanly?" cheaply (merge queues, `grit merge --check`). Use `write_merge_tree` when you need the result itself: it is the library form of `git merge-tree --write-tree`. A clean result can be committed straight away with `NewCommit::new(tree, ..).with_parents(vec![ours, theirs])`. On conflicts the stored tree holds conflict markers in the conflicted files, and [`WrittenMergeTree::stages`](rustdoc:grit_lib::porcelain::merge::WrittenMergeTree) lists the `mode`, `oid`, `stage` and `path` rows Git prints after the tree id. Set `ours_label` and `theirs_label` to choose the conflict-marker labels.

`write_merge_tree` always merges (the fast-forward option is ignored), merges several merge bases into a virtual base first (stage 1 entries then come from it, as with `git merge-tree`), and refuses commits with no common ancestor with `Error::UnrelatedHistories` unless `allow_unrelated_histories` is set.

## Directory renames

When one side moves every file out of a directory and the other side adds a file inside it, the new file most likely belongs in the directory's new home. Tree merges detect this the way Git's `ort` merge does, controlled by [`DirectoryRenameMode`](rustdoc:grit_lib::diff::merge_trees::DirectoryRenameMode):

| Mode | `merge.directoryRenames` | Result |
| --- | --- | --- |
| [`Conflict`](rustdoc:grit_lib::diff::merge_trees::DirectoryRenameMode::Conflict) (default) | `conflict` or unset | The file moves to the new directory and stays unmerged at the adding side's stage (stage 3 when theirs added it), so the merge stops for confirmation. |
| [`Apply`](rustdoc:grit_lib::diff::merge_trees::DirectoryRenameMode::Apply) | `true` | The file moves and the merge is clean. |
| [`Off`](rustdoc:grit_lib::diff::merge_trees::DirectoryRenameMode::Off) | `false` | The file stays in the old directory. |

A directory counts as renamed only when the side removed it entirely; the destination is the directory that received the most of its files, and a tie is a split that moves nothing. A destination path the adding side already uses, or that several new files would share, blocks the move; a file the base or the renaming side has at the destination is instead three-way merged with the moved file there. Splits and blocked moves are conflicts in `Conflict` and `Apply` mode even though no index entry is unmerged. All of them are listed in [`TreeMergeOutput::directory_rename_conflicts`](rustdoc:grit_lib::diff::merge_trees::TreeMergeOutput::directory_rename_conflicts) and reported by [`conflicts_from_tree_merge`](rustdoc:grit_lib::operation::conflicts_from_tree_merge) as the `directory_rename_*` [`ConflictKind`](rustdoc:grit_lib::operation::ConflictKind)s.

[`merge_trees_three_way`](rustdoc:grit_lib::diff::merge_trees::merge_trees_three_way) takes the mode as an argument; read the configured one with [`DirectoryRenameMode::from_repo`](rustdoc:grit_lib::diff::merge_trees::DirectoryRenameMode::from_repo), which fails with [`ConfigError::InvalidDirectoryRenames`](rustdoc:grit_lib::error::ConfigError::InvalidDirectoryRenames) for an unknown value. [`MergePredictionOptions::directory_renames`](rustdoc:grit_lib::porcelain::merge::MergePredictionOptions::directory_renames) and [`MergeOptions::directory_renames`](rustdoc:grit_lib::porcelain::merge::MergeOptions::directory_renames) override the config for one merge; pick, revert, rebase and history edits always use the config.

## Example

The program resolves `ours` and `theirs` refs, stores the merge with `write_merge_tree`, prints the merged tree oid (compare with `git merge-tree --write-tree`) and, when the merge is clean, commits it with both parents:

<!-- include: grit-examples/src/bin/guide_merge.rs -->

The integration test builds non-conflicting `ours` / `theirs` branches, asserts the merged tree matches Git’s oracle, and runs `git fsck --strict` on the resulting merge commit.

## Integrating a branch (`integrate_branch`)

[`integrate_branch`](rustdoc:grit_lib::porcelain::merge::integrate_branch) merges one commit into a local branch and updates the index and work tree (or records merge-in-progress state on conflicts). Pass `into_ref` as either a full ref (for example `refs/heads/main`) or a short branch name (for example `main` or `feature/topic`); short names map to `refs/heads/{name}` and missing branches return [`Error::BranchNotFound`](rustdoc:grit_lib::error::Error::BranchNotFound). The `label` argument names the other side in the default merge message (`Merge {label}` unless [`MergeOptions::message`](rustdoc:grit_lib::porcelain::merge::MergeOptions::message) overrides it) and in branch reflog lines (`merge {label}`, or `merge {label}: fast-forward` on fast-forward).

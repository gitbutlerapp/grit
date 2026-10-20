---
title: Diff
summary: Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.
---

Diffing in grit-lib centers on the [`diff`](rustdoc:grit_lib::diff) domain module (tree/content diff, merge helpers, patch-ids). Prefer `Repository::diff` (`Diff`) for tree, index, and worktree comparisons on one open handle. Results are `DiffEntry` rows with a `DiffStatus` letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](../../diff/) and `porcelain::status` use before formatting output. See [API layout](../api-layout/) for the flat-path map.

## Pickaxe

[`Pickaxe`](rustdoc:grit_lib::diff::Pickaxe) implements Git's pickaxe filters for revision walks: [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::Pickaxe::occurrences) (`-S`, occurrence-count change per file) and [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::Pickaxe::line_grep) (`-G`, regex on added/removed lines). Call [`Pickaxe::matches_commit`](rustdoc:grit_lib::diff::Pickaxe::matches_commit) to test one commit against its first parent; wire the filter through [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions) for `grit log` / `grit revs`.

## Tree-to-tree

`diff_trees` compares two tree objects recursively and returns changed paths. Pass `None` for either side to diff against an empty tree. `diff_trees_show_tree_entries` can emit tree objects themselves (Git’s `diff-tree -t` behavior).

Commit-to-commit diffs resolve each commit’s tree with `parse_commit`, then call `diff_trees` on the parent and child trees.

## Index-to-worktree

`diff_index_to_worktree` compares the index to files on disk. `diff_index_to_worktree_with_options` adds index mtime, submodule/gitlink handling, broken-gitlink detection, and an optional repository git-dir override. `porcelain::add::stage` uses `diff_index_to_worktree_for_staging` internally so staging sees the same dirty paths as status.

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and pass them to [`unified_diff`](rustdoc:grit_lib::diff::unified_diff) (histogram algorithm, Git-compatible hunks). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus optional unified bodies.

## Porcelain models

- **Status** — combines index vs HEAD and index vs worktree scans into structured sections (the CLI maps them to default / `--json` / `--markdown` output).
- **Add** — uses diff results to decide which paths to hash and stage.
- **Diff command** — tree-to-tree for commit ranges, index-to-worktree for uncommitted changes, then blob-level rendering for text files.

## Example

The program below diffs the latest commit against its parent (tree-to-tree), checks whether the work tree is dirty, and counts unified-diff lines for one modified blob:

<!-- include: grit-examples/src/bin/guide_diff.rs -->

Requires a repository whose `HEAD` has a parent (at least two commits):

```bash
cargo run --bin guide_diff /path/to/repo
git -C /path/to/repo diff --name-status HEAD~1 HEAD
```

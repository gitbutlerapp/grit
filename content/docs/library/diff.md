---
title: Diff
summary: Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.
---

Diffing in grit-lib centers on the [`diff`](rustdoc:grit_lib::diff) domain module (tree/content diff, merge helpers, patch-ids). Prefer `Repository::diff` (`Diff`) for tree, index, and worktree comparisons on one open handle. Results are `DiffEntry` rows with a `DiffStatus` letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](../../diff/) and `porcelain::status` use before formatting output. See [API layout](../api-layout/) for the flat-path map.

## Selecting what to diff

Porcelain and the CLI resolve user arguments into a [`DiffTarget`](rustdoc:grit_lib::diff::DiffTarget) with [`resolve_diff_arguments`](rustdoc:grit_lib::diff::resolve_diff_arguments): uncommitted changes (`WorktreeVsHead`), `--staged` (`IndexVsHead`), `--unstaged` (`WorktreeVsIndex`), `--against <ref>` (`WorktreeVsTree`), a single commit (`Commit`), or a commit range (`Trees`). Call [`Diff::files`](rustdoc:grit_lib::diff::Diff::files) with normalized pathspecs and [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions) to obtain [`FileDiff`](rustdoc:grit_lib::diff::FileDiff) rows (structured hunks plus paths, modes, and OIDs).

## Tree-to-tree

`diff_trees` compares two tree objects recursively and returns changed paths. Pass `None` for either side to diff against an empty tree. `diff_trees_show_tree_entries` can emit tree objects themselves (Git’s `diff-tree -t` behavior).

Commit-to-commit diffs resolve each commit’s tree with `parse_commit`, then call `diff_trees` on the parent and child trees.

## Index-to-worktree

`diff_index_to_worktree` compares the index to files on disk. `diff_index_to_worktree_with_options` adds index mtime, submodule/gitlink handling, broken-gitlink detection, and an optional repository git-dir override. `porcelain::add::stage` uses `diff_index_to_worktree_for_staging` internally so staging sees the same dirty paths as status.

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and pass them to [`unified_diff`](rustdoc:grit_lib::diff::unified_diff) or the structured helpers in [`structured`](rustdoc:grit_lib::diff::structured). Line-level hunks honor one shared [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions) value (algorithm, context, indent heuristic, renames, optional word-level inline emphasis) whether you emit unified patch text or structured hunks for the CLI. Defaults match Git (`Myers`, `-U3`, indent heuristic on, rename similarity 50). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus structured or unified bodies.

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

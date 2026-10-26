---
title: Diff
summary: Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.
---

Diffing in grit-lib centers on the [`diff`](rustdoc:grit_lib::diff) domain module (tree/content diff, merge helpers, patch-ids). Prefer `Repository::diff` (`Diff`) for tree, index, and worktree comparisons on one open handle. Results are `DiffEntry` rows with a `DiffStatus` letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](../../diff/) and `porcelain::status` use before formatting output. See [API layout](../api-layout/) for the flat-path map.

## Pickaxe

[`Pickaxe`](rustdoc:grit_lib::diff::pickaxe::Pickaxe) implements Git's pickaxe filters for revision walks: [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::occurrences) (`-S`, occurrence-count change per file) and [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::line_grep) (`-G`, regex on added/removed lines). Call [`Pickaxe::matches_commit`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::matches_commit) to test one commit against its first parent; wire the filter through [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions) for `grit log` / `grit revs`.

## Choosing what to compare

[`resolve_diff_request`](rustdoc:grit_lib::diff::resolve_diff_request) turns diff operands as a user typed them ([`DiffRequest`](rustdoc:grit_lib::diff::DiffRequest): `--staged`, `--unstaged`, `--against`, operands before and after `--`, and pathspec-file entries) into a [`DiffTarget`](rustdoc:grit_lib::diff::DiffTarget) plus repository-relative pathspecs. This is the resolver [`grit diff`](../../commands/diff.md) uses. Operands that are not revisions but name known paths become pathspecs, so `file.txt` and `HEAD~1 file.txt` need no `--`. One revision selects that commit's own patch, `A..B` and `A...B` select a tree diff (merge base of `A` and `B` against `B` for three dots), and two revisions compare their trees. Failures are typed [`ResolveDiffArgumentsError`](rustdoc:grit_lib::diff::ResolveDiffArgumentsError) variants: flag conflicts, `UnresolvedRevision` (carrying the library cause), `NotACommit`, `NoMergeBase`, and pathspec errors. Hand the target to `Repository::diff().files(...)` for structured file diffs. [`resolve_commit_diff_pathspecs`](rustdoc:grit_lib::diff::resolve_commit_diff_pathspecs) normalizes path limits for commit diffs on their own (as `grit show` and `grit log -p` do), without requiring the paths to exist in the checkout.

## Tree-to-tree

`diff_trees` compares two tree objects recursively and returns changed paths. Pass `None` for either side to diff against an empty tree. `diff_trees_show_tree_entries` can emit tree objects themselves (Git’s `diff-tree -t` behavior).

Commit-to-commit diffs resolve each commit’s tree with [`Odb::commit_tree`](rustdoc:grit_lib::objects::odb::Odb::commit_tree), then call `diff_trees` on the parent and child trees.

## Index-to-worktree

`diff_index_to_worktree` compares the index to files on disk. `diff_index_to_worktree_with_options` adds index mtime, submodule/gitlink handling, broken-gitlink detection, and an optional repository git-dir override. `porcelain::add::stage` uses `diff_index_to_worktree_for_staging` internally so staging sees the same dirty paths as status.

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and pass them to [`unified_diff`](rustdoc:grit_lib::diff::unified_diff) or the structured helpers in [`structured`](rustdoc:grit_lib::diff::structured). For tree diffs meant for `git apply`, stream [`write_patch`](rustdoc:grit_lib::diff::write_patch) over a `DiffEntry` slice with [`PatchTextOptions`](rustdoc:grit_lib::diff::PatchTextOptions) (same headers and `GIT binary patch` bodies as `git diff --binary`). For index and work-tree diffs, [`write_patch_resolved`](rustdoc:grit_lib::diff::write_patch_resolved) takes [`PatchResolvedSide`](rustdoc:grit_lib::diff::PatchResolvedSide) values whose sides are a [`PatchSide`](rustdoc:grit_lib::diff::PatchSide): bytes you already hold, a blob in the object database, or the work-tree file (read through clean filters and line-ending rules). Binary files get full ids of the written content on their `index` line, which `git apply` requires. Line-level hunks honor one shared [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions) value (algorithm, context, indent heuristic, renames, optional word-level inline emphasis) whether you emit unified patch text or structured hunks for the CLI. Defaults match Git (`Myers`, `-U3`, indent heuristic on, rename similarity 50). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus structured or unified bodies; [`grit diff --patch-text`](../../commands/diff.md) calls `write_patch` for revisions and `write_patch_resolved` for staged and work-tree changes.

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

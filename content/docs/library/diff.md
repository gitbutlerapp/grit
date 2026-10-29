---
title: Diff
summary: Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.
---

Diffing in grit-lib centers on the [`diff`](rustdoc:grit_lib::diff) domain module (tree/content diff, merge helpers, patch-ids). Prefer `Repository::diff` (`Diff`) for tree, index, and worktree comparisons on one open handle. Results are `DiffEntry` rows with a `DiffStatus` letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](../../diff/) and `porcelain::status` use before formatting output. See [API layout](../api-layout/) for the flat-path map.

## Pickaxe

[`Pickaxe`](rustdoc:grit_lib::diff::pickaxe::Pickaxe) implements Git's pickaxe filters for revision walks: [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::occurrences) (`-S`, occurrence-count change per file) and [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::line_grep) (`-G`, regex on added/removed lines). Call [`Pickaxe::matches_commit`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::matches_commit) to test one commit against its first parent; wire the filter through [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions) for `grit log` / `grit revs`.

## Choosing what to compare

[`resolve_diff_request`](rustdoc:grit_lib::diff::resolve_diff_request) turns diff operands as a user typed them ([`DiffRequest`](rustdoc:grit_lib::diff::DiffRequest): `--staged`, `--unstaged`, `--against`, operands before and after `--`, and pathspec-file entries) into a [`DiffTarget`](rustdoc:grit_lib::diff::DiffTarget) plus repository-relative pathspecs. This is the resolver [`grit diff`](../../commands/diff.md) uses. Operands that are not revisions but name known paths become pathspecs, so `file.txt` and `HEAD~1 file.txt` need no `--`. One revision selects that commit's own patch, `A..B` and `A...B` select a tree diff (merge base of `A` and `B` against `B` for three dots), and two revisions compare their trees. Failures are typed [`ResolveDiffArgumentsError`](rustdoc:grit_lib::diff::ResolveDiffArgumentsError) variants: flag conflicts, `UnresolvedRevision` (carrying the library cause), `NotACommit`, `NoMergeBase`, and pathspec errors. When explicit paths or pathspec-file entries accompany an unresolved revision, the resolver returns `UnresolvedRevision`; only a lone unresolved operand without path limits falls back to a worktree path. Hand the target to `Repository::diff().files(...)` for structured file diffs. [`resolve_commit_diff_pathspecs`](rustdoc:grit_lib::diff::resolve_commit_diff_pathspecs) normalizes path limits for commit diffs on their own (as `grit show` and `grit log -p` do), without requiring the paths to exist in the checkout.

## Tree-to-tree

`diff_trees` compares two tree objects recursively and returns changed paths. Pass `None` for either side to diff against an empty tree. `diff_trees_show_tree_entries` can emit tree objects themselves (Git’s `diff-tree -t` behavior).

`Diff::tree_to_tree` reads tree metadata only and prunes identical subtree object ids without opening them. The walk retains active directory frames and the ODB’s bounded parsed-tree cache; completed siblings do not accumulate in a second cache, and trees read for that cache are not also copied into the ODB’s raw-object cache. The returned `Vec<DiffEntry>` owns changed paths, modes and ids, so result memory still grows with the number of changes. `build_file_diffs` consumes these entries for `NameOnly` and `NameStatus`, reusing their path and mode strings without reading blob content; [`FileDiff::from_metadata`](rustdoc:grit_lib::diff::ListedFileDiff::from_metadata) builds the same row from one entry, so a caller can map a list into its own output type in a single pass. Rename detection is separate and may read blobs for similarity scoring. When a list has no added/deleted pair to score, [`detect_renames_with_policy`](rustdoc:grit_lib::diff::detect_renames_with_policy) returns it unchanged without reading content or copying it. Diff lists are in Git’s order, raw path bytes; [`sort_entries_by_path`](rustdoc:grit_lib::diff::sort_entries_by_path) restores that order after filtering or merging lists and costs one comparison pass when the list is already ordered. See the [tree metadata measurements](../../benchmarks/#tree-metadata-diff-2026-10-27) and the [CLI follow-up](../../benchmarks/#tree-metadata-diff-through-the-cli-2026-10-27).

Commit-to-commit diffs resolve each commit’s tree with [`Odb::commit_tree`](rustdoc:grit_lib::objects::odb::Odb::commit_tree), then call `diff_trees` on the parent and child trees.

## Index-to-worktree

`diff_index_to_worktree` compares the index to files on disk. `diff_index_to_worktree_with_options` adds index mtime, submodule/gitlink handling, broken-gitlink detection, and an optional repository git-dir override. `porcelain::add::stage` uses `diff_index_to_worktree_for_staging` internally so staging sees the same dirty paths as status.

## Staged, unstaged, and working-tree changes

[`Diff::uncommitted`](rustdoc:grit_lib::diff::Diff::uncommitted) loads the index once and returns an [`UncommittedDiff`](rustdoc:grit_lib::diff::UncommittedDiff) for the comparisons `grit diff` makes without revisions. [`staged_entries`](rustdoc:grit_lib::diff::UncommittedDiff::staged_entries) compares the index with `HEAD`'s tree, [`unstaged_entries`](rustdoc:grit_lib::diff::UncommittedDiff::unstaged_entries) compares the working tree with the index, and [`worktree_entries`](rustdoc:grit_lib::diff::UncommittedDiff::worktree_entries) compares the working tree and index together with any tree (`HEAD`'s for `grit diff`, another revision's for `--against`). Each returns sorted `DiffEntry` rows without reading blob content, so filter them by pathspec first and then load only what you need. [`load_worktree_changes`](rustdoc:grit_lib::diff::UncommittedDiff::load_worktree_changes) reads new sides from the working tree (clean filters and line-ending rules applied, a gitlink shown as its checked-out commit with `-dirty` when modified); [`load_object_changes`](rustdoc:grit_lib::diff::load_object_changes) reads both sides from the object database in parallel, for staged or tree entries. Both return [`FileChange`](rustdoc:grit_lib::diff::FileChange) rows, which give structured hunks with [`structured`](rustdoc:grit_lib::diff::FileChange::structured) or line counts with [`line_stat`](rustdoc:grit_lib::diff::FileChange::line_stat). One `UncommittedDiff` reuses its index and worktree rules across all of these calls. The working-tree methods return `Error::RequiresWorkTree` in a bare repository, and [`resolve_diff_request`](rustdoc:grit_lib::diff::resolve_diff_request) rejects a working-tree target there with `ResolveDiffArgumentsError::NoWorkTree`.

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and pass them to [`unified_diff`](rustdoc:grit_lib::diff::unified_diff) or the structured helpers in [`structured`](rustdoc:grit_lib::diff::structured). For tree diffs meant for `git apply`, stream [`write_patch`](rustdoc:grit_lib::diff::write_patch) over a `DiffEntry` slice with [`PatchTextOptions`](rustdoc:grit_lib::diff::PatchTextOptions) (same headers and `GIT binary patch` bodies as `git diff --binary`). For index and work-tree diffs, [`write_patch_resolved`](rustdoc:grit_lib::diff::write_patch_resolved) takes [`PatchResolvedSide`](rustdoc:grit_lib::diff::PatchResolvedSide) values whose sides are a [`PatchSide`](rustdoc:grit_lib::diff::PatchSide): bytes you already hold, a blob in the object database, or the work-tree file (read through clean filters and line-ending rules). Binary files get full ids of the written content on their `index` line, which `git apply` requires. Line-level hunks honor one shared [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions) value (algorithm, context, indent heuristic, renames, optional word-level inline emphasis) whether you emit unified patch text or structured hunks for the CLI. Defaults match Git (`Myers`, `-U3`, indent heuristic on, rename similarity 50). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus structured or unified bodies; [`grit diff --patch-text`](../../commands/diff.md) calls `write_patch` for revisions and `write_patch_resolved` for staged and work-tree changes.

## Diffstat

[`DiffStat`](rustdoc:grit_lib::diff::DiffStat) is the data behind `--stat`, `--numstat` and `--shortstat`. [`DiffStat::from_files`](rustdoc:grit_lib::diff::DiffStat::from_files) takes the rows from `build_file_diffs` at any detail level: each row's counts come from [`FileDiff::line_stat`](rustdoc:grit_lib::diff::ListedFileDiff::line_stat), which uses the stat when one was computed and otherwise counts the structured or combined hunk lines, so a patch-mode listing and a stat-mode listing give the same diffstat. [`totals`](rustdoc:grit_lib::diff::DiffStat::totals) sums the rows. To draw bars, scale each row with [`LineStat::bar`](rustdoc:grit_lib::diff::LineStat::bar) against [`max_changes`](rustdoc:grit_lib::diff::DiffStat::max_changes) and a column budget; the returned [`StatBar`](rustdoc:grit_lib::diff::StatBar) gives `+` and `-` column counts. `grit diff --stat`, `grit show`, `grit log --stat` and `grit stash show` render this model.

```rust
use grit_lib::diff::{build_file_diffs, DiffDetail, DiffOptions, DiffStat};
use grit_lib::parallel::ThreadPolicy;

let entries = repo.diff().tree_to_tree(Some(&old_tree), Some(&new_tree))?;
let config = repo.config()?;
let files = build_file_diffs(
    repo.odb(),
    entries,
    DiffDetail::Stat,
    DiffOptions::default(),
    config.as_ref(),
    &ThreadPolicy::default(),
)?
.files;
let stat = DiffStat::from_files(&files);
for file in stat.files() {
    let bar = file.lines.bar(stat.max_changes(), 40);
    println!("{} | {}{}", file.path, "+".repeat(bar.plus), "-".repeat(bar.minus));
}
let totals = stat.totals();
println!("{} files, +{} -{}", totals.files_changed, totals.insertions, totals.deletions);
```

Under any whitespace rule (`-w`, `-b`, `--ignore-space-at-eol`, `--ignore-blank-lines`), Git leaves out of `--stat` any modified text file whose changes were all ignored. [`FileDiff::changes_all_ignored`](rustdoc:grit_lib::diff::ListedFileDiff::changes_all_ignored) tells you which rows those are. Additions, deletions, renames, mode changes and binary files stay listed. For a stash, [`RepositoryStash::diff_stat`](rustdoc:grit_lib::repo_handles::RepositoryStash::diff_stat) returns the finished `DiffStat` of `stash@{n}` against its base (`git stash show --stat`). It counts lines without building hunks and applies that whitespace rule.

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

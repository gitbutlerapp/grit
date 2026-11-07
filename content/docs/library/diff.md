---
title: Diff
summary: Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.
---

Diffing in grit-lib centers on the [`diff`](rustdoc:grit_lib::diff) domain module (tree/content diff, merge helpers, patch-ids). Prefer `Repository::diff` (`Diff`) for tree, index, and worktree comparisons on one open handle. Results are `DiffEntry` rows with a `DiffStatus` letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](../../diff/) and `porcelain::status` use before formatting output. See [API layout](../api-layout/) for the flat-path map.

Two public structs carry per-file diff output: [`FileDiff`](rustdoc:grit_lib::diff::FileDiff) from [`Diff::files`](rustdoc:grit_lib::diff::Diff::files) always includes structured hunks when line diff ran; [`ListedFileDiff`](rustdoc:grit_lib::diff::ListedFileDiff) from [`Diff::files_for_entries`](rustdoc:grit_lib::diff::Diff::files_for_entries) is the stat-or-patch row where structured hunks and line counts are filled according to [`DiffDetail`](rustdoc:grit_lib::diff::DiffDetail).

## Entry points

Every diff is a method on the [`Diff`](rustdoc:grit_lib::diff::Diff) handle from `repo.diff()`. The metadata diffs list changed paths without loading contents. They take one [`TreeDiffOptions`](rustdoc:grit_lib::diff::TreeDiffOptions) and return a [`DiffEntriesResult`](rustdoc:grit_lib::diff::DiffEntriesResult): the entries, a notice when `diff.renameLimit` skipped inexact rename scoring, and whether an index-to-work-tree scan refreshed stat data. Configuration (`diff.renameLimit`, attributes, `core.autocrlf`, thread settings) is read from the repository inside the handle.

| Method | Compares | Git equivalent |
| --- | --- | --- |
| [`tree_to_tree`](rustdoc:grit_lib::diff::Diff::tree_to_tree) | two trees (either may be the empty tree) | `git diff-tree -r A B` |
| [`commits`](rustdoc:grit_lib::diff::Diff::commits) | the trees of two commits | `git diff-tree -r A B` on commits |
| [`index_to_tree`](rustdoc:grit_lib::diff::Diff::index_to_tree) | the index against a tree | `git diff --cached` |
| [`index_to_worktree`](rustdoc:grit_lib::diff::Diff::index_to_worktree) | the work tree against the index | `git diff-files` |
| [`tree_to_worktree`](rustdoc:grit_lib::diff::Diff::tree_to_worktree) | the work tree against a tree, through the index | `git diff <tree>` |
| [`detect_renames`](rustdoc:grit_lib::diff::Diff::detect_renames) | pairs entries you already have | `diffcore` rename, copy and break passes |

`TreeDiffOptions::default()` compares every path and pairs nothing. Its fields, each with a `with_<field>` setter:

| Field | Meaning |
| --- | --- |
| [`renames`](rustdoc:grit_lib::diff::TreeDiffOptions::renames) | [`RenameDetection::Similarity(n)`](rustdoc:grit_lib::diff::RenameDetection) is Git `-M<n>`; `Similarity(100)` pairs identical content only |
| [`copies`](rustdoc:grit_lib::diff::TreeDiffOptions::copies) | [`CopyDetection::Similarity(n)`](rustdoc:grit_lib::diff::CopyDetection) is `-C<n>` (sources are paths changed in the same diff); `FindHarder(n)` adds `--find-copies-harder` (every old path is a source) |
| [`break_rewrites`](rustdoc:grit_lib::diff::TreeDiffOptions::break_rewrites) | [`BreakRewrites::DEFAULT`](rustdoc:grit_lib::diff::BreakRewrites::DEFAULT) is a bare `-B`; with renames or copies on, broken pairs are split so their halves can pair elsewhere |
| [`pathspec`](rustdoc:grit_lib::diff::TreeDiffOptions::pathspec) | a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec); tree walks skip directories it cannot reach and work-tree scans read only the paths it selects |
| [`show_tree_entries`](rustdoc:grit_lib::diff::TreeDiffOptions::show_tree_entries) | also report changed subtrees (`diff-tree -t`); tree and commit diffs |
| [`prefix`](rustdoc:grit_lib::diff::TreeDiffOptions::prefix) | prepended to reported paths when the trees are subtrees; tree and commit diffs |
| [`submodules`](rustdoc:grit_lib::diff::TreeDiffOptions::submodules) | [`SubmoduleIgnoreMode`](rustdoc:grit_lib::diff::SubmoduleIgnoreMode) (`--ignore-submodules=none|untracked|dirty|all`); `All` omits gitlink paths |
| [`threads`](rustdoc:grit_lib::diff::TreeDiffOptions::threads) | a [`ThreadPolicy`](rustdoc:grit_lib::parallel::ThreadPolicy) for rename scoring and stat workers; `None` uses the repository's |
| [`refresh_index_stat`](rustdoc:grit_lib::diff::TreeDiffOptions::refresh_index_stat), [`for_status`](rustdoc:grit_lib::diff::TreeDiffOptions::for_status) | index-to-work-tree only: adopt fresh stat data in the same scan, and status shortcuts that skip reading files whose size changed |

```rust
use grit_lib::diff::{CopyDetection, RenameDetection, TreeDiffOptions};
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

// `git diff-tree -r -M50 -C50 --find-copies-harder old new -- src/`
let options = TreeDiffOptions::default()
    .with_renames(RenameDetection::Similarity(50))
    .with_copies(CopyDetection::FindHarder(50))
    .with_pathspec(Some(Pathspec::scoped(["src/"], PathspecGlobalFlags::default())));
let result = repo.diff().tree_to_tree(Some(&old_tree), Some(&new_tree), &options)?;
if let Some(skipped) = &result.inexact_rename_skipped {
    eprintln!("raise diff.renameLimit to {}", skipped.suggested_limit);
}
for entry in &result.entries {
    println!("{}\t{}", entry.status.letter(), entry.path());
}
```

The content methods turn entries into hunks and line counts under a [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions): [`files_for_entries`](rustdoc:grit_lib::diff::Diff::files_for_entries) at any [`DiffDetail`](rustdoc:grit_lib::diff::DiffDetail), [`for_each_file`](rustdoc:grit_lib::diff::Diff::for_each_file) one patch row at a time, [`stat`](rustdoc:grit_lib::diff::Diff::stat) for a [`DiffStat`](rustdoc:grit_lib::diff::DiffStat), and [`structured`](rustdoc:grit_lib::diff::Diff::structured) / [`for_each_structured`](rustdoc:grit_lib::diff::Diff::for_each_structured) for [`TreeDiffStructured`](rustdoc:grit_lib::diff::TreeDiffStructured) rows in input order. [`commit_files`](rustdoc:grit_lib::diff::Diff::commit_files) diffs one commit against its parents (`grit show`), [`object_changes`](rustdoc:grit_lib::diff::Diff::object_changes) loads both sides of stored-object entries, and [`hunk_bundle`](rustdoc:grit_lib::diff::Diff::hunk_bundle) gathers unstaged hunks for hunk-level staging.

## Pickaxe

[`Pickaxe`](rustdoc:grit_lib::diff::Pickaxe) implements Git's pickaxe filters for revision walks: [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::Pickaxe::occurrences) (`-S`, occurrence-count change per file) and [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::Pickaxe::line_grep) (`-G`, regex on added/removed lines). Call [`Pickaxe::matches_commit`](rustdoc:grit_lib::diff::Pickaxe::matches_commit) to test one commit against its first parent; wire the filter through [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::RevWalkListOptions) for `grit log` / `grit revs`.

## Choosing what to compare

[`Diff::resolve_request`](rustdoc:grit_lib::diff::Diff::resolve_request) turns diff operands as a user typed them ([`DiffRequest`](rustdoc:grit_lib::diff::DiffRequest): `--staged`, `--unstaged`, `--against`, operands before and after `--`, and pathspec-file entries) into a [`DiffTarget`](rustdoc:grit_lib::diff::DiffTarget) plus a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) on [`DiffArguments::pathspec`](rustdoc:grit_lib::diff::DiffArguments::pathspec). This is the resolver [`grit diff`](../../commands/diff.md) uses. Operands that are not revisions but name known paths become pathspecs, so `file.txt` and `HEAD~1 file.txt` need no `--`. One revision selects that commit's own patch, `A..B` and `A...B` select a tree diff (merge base of `A` and `B` against `B` for three dots), and two revisions compare their trees. Failures are typed [`ResolveDiffArgumentsError`](rustdoc:grit_lib::diff::ResolveDiffArgumentsError) variants: flag conflicts, `UnresolvedRevision` (carrying the library cause), `NotACommit`, `NoMergeBase`, and pathspec errors. When explicit paths or pathspec-file entries accompany an unresolved revision, the resolver returns `UnresolvedRevision`; only a lone unresolved operand without path limits falls back to a worktree path. Hand the target and path limit to [`Diff::files`](rustdoc:grit_lib::diff::Diff::files) (`&Pathspec`) for structured file diffs. [`Diff::resolve_pathspec`](rustdoc:grit_lib::diff::Diff::resolve_pathspec) returns a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) for commit diffs on their own (as `grit show` and `grit log -p` do), without requiring the paths to exist in the checkout.

## Tree-to-tree

[`Diff::tree_to_tree`](rustdoc:grit_lib::diff::Diff::tree_to_tree) reads tree metadata only and prunes identical subtree object ids without opening them. The walk retains active directory frames and the ODB’s bounded parsed-tree cache; completed siblings do not accumulate in a second cache, and trees read for that cache are not also copied into the ODB’s raw-object cache. The returned entries own changed paths, modes and ids, so result memory still grows with the number of changes. Rename, copy and break pairing run only when `TreeDiffOptions` asks for them; when a list has no added/deleted pair to score, rename detection returns it unchanged without reading content. [`Diff::files_for_entries`](rustdoc:grit_lib::diff::Diff::files_for_entries) applies [`DiffOptions::renames`](rustdoc:grit_lib::diff::DiffOptions::renames) (default similarity 50, Git `-M`) before stat or patch work; pass [`RenameDetection::Off`](rustdoc:grit_lib::diff::RenameDetection::Off) when the entries are already paired. For metadata-only output, [`FileDiff::from_metadata`](rustdoc:grit_lib::diff::ListedFileDiff::from_metadata) builds a row from one entry without line-diff blob reads. Diff lists are in Git’s order, raw path bytes; [`sort_entries_by_path`](rustdoc:grit_lib::diff::sort_entries_by_path) restores that order after filtering or merging lists and costs one comparison pass when the list is already ordered. See the [tree metadata measurements](../../benchmarks/#tree-metadata-diff-2026-10-27) and the [CLI follow-up](../../benchmarks/#tree-metadata-diff-through-the-cli-2026-10-27).

[`Diff::commits`](rustdoc:grit_lib::diff::Diff::commits) resolves each commit’s tree and diffs them the same way; it returns `Error::InvalidRef` when an id is not a commit.

## Index-to-worktree

[`Diff::index_to_worktree`](rustdoc:grit_lib::diff::Diff::index_to_worktree) compares the files on disk with the index. Entries whose stat data matches are not read, and racily clean entries (modified within the index file's timestamp) are hashed to be sure. The work-tree side of a changed file carries a null `new_oid` unless its content had to be hashed. Gitlinks follow [`TreeDiffOptions::submodules`](rustdoc:grit_lib::diff::TreeDiffOptions::submodules). With [`refresh_index_stat`](rustdoc:grit_lib::diff::TreeDiffOptions::refresh_index_stat) the in-memory index adopts fresh stat data for unchanged entries in the same scan, and [`DiffEntriesResult::index_stat_refreshed`](rustdoc:grit_lib::diff::DiffEntriesResult::index_stat_refreshed) says whether writing it back is worthwhile. [`Diff::tree_to_worktree`](rustdoc:grit_lib::diff::Diff::tree_to_worktree) compares the work tree with any tree through the index, reading work-tree bytes through attributes and clean filters. Both return [`RepositoryError::RequiresWorkTree`](rustdoc:grit_lib::repo::RepositoryError::RequiresWorkTree) in a bare repository.

## Staged, unstaged, and working-tree changes

[`Diff::uncommitted`](rustdoc:grit_lib::diff::Diff::uncommitted) loads the index once and returns an [`UncommittedDiff`](rustdoc:grit_lib::diff::UncommittedDiff) for the comparisons `grit diff` makes without revisions. [`staged_entries`](rustdoc:grit_lib::diff::UncommittedDiff::staged_entries) compares the index with `HEAD`'s tree, [`unstaged_entries`](rustdoc:grit_lib::diff::UncommittedDiff::unstaged_entries) compares the working tree with the index, and [`worktree_entries`](rustdoc:grit_lib::diff::UncommittedDiff::worktree_entries) compares the working tree and index together with any tree (`HEAD`'s for `grit diff`, another revision's for `--against`). Each returns sorted `DiffEntry` rows without reading blob content, so filter them by pathspec first and then load only what you need. [`load_worktree_changes`](rustdoc:grit_lib::diff::UncommittedDiff::load_worktree_changes) reads new sides from the working tree (clean filters and line-ending rules applied, a gitlink shown as its checked-out commit with `-dirty` when modified); [`Diff::object_changes`](rustdoc:grit_lib::diff::Diff::object_changes) reads both sides from the object database in parallel, for staged or tree entries. Both return [`FileChange`](rustdoc:grit_lib::diff::FileChange) rows, which give structured hunks with [`structured`](rustdoc:grit_lib::diff::FileChange::structured) or line counts with [`line_stat`](rustdoc:grit_lib::diff::FileChange::line_stat). One `UncommittedDiff` reuses its index and worktree rules across all of these calls. The working-tree methods return [`RepositoryError::RequiresWorkTree`](rustdoc:grit_lib::repo::RepositoryError::RequiresWorkTree) (as [`Error::Repository`](rustdoc:grit_lib::error::Error::Repository)) in a bare repository, and [`Diff::resolve_request`](rustdoc:grit_lib::diff::Diff::resolve_request) rejects a working-tree target there with `ResolveDiffArgumentsError::NoWorkTree`.

To go straight from a resolved [`DiffTarget`](rustdoc:grit_lib::diff::DiffTarget) to its changed paths, call [`UncommittedDiff::entries`](rustdoc:grit_lib::diff::UncommittedDiff::entries) with the target, a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec), and a `DiffOptions`: it picks the comparison above (with `HEAD`'s tree for `WorktreeVsHead`), keeps the entries within the pathspecs, and applies [`DiffOptions::filter`](rustdoc:grit_lib::diff::DiffOptions::filter) (Git `--diff-filter`) after rename detection. Load the result on the same handle. For commit and range targets, [`Diff::entries`](rustdoc:grit_lib::diff::Diff::entries) does the same without reading the index. This is how `grit diff` and `grit diff --patch-text` choose their entries. To give staged and unstaged hunks stable ids, [`UncommittedDiff::peer_changes`](rustdoc:grit_lib::diff::UncommittedDiff::peer_changes) loads the changes on the other side of the index for the same paths, ready to pass as the peer hunks of [`hunk_ids`](rustdoc:grit_lib::diff::hunk_ids).

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and pass them to [`unified_diff`](rustdoc:grit_lib::diff::unified_diff) or the structured helpers in [`structured`](rustdoc:grit_lib::diff::structured). For tree diffs meant for `git apply`, stream [`write_patch`](rustdoc:grit_lib::diff::write_patch) over a `DiffEntry` slice with [`PatchTextOptions`](rustdoc:grit_lib::diff::PatchTextOptions) (same headers and `GIT binary patch` bodies as `git diff --binary`). For index and work-tree diffs, [`write_patch_resolved`](rustdoc:grit_lib::diff::write_patch_resolved) takes [`PatchResolvedSide`](rustdoc:grit_lib::diff::PatchResolvedSide) values whose sides are a [`PatchSide`](rustdoc:grit_lib::diff::PatchSide): bytes you already hold, a blob in the object database, or the work-tree file (read through clean filters and line-ending rules). Binary files get full ids of the written content on their `index` line, which `git apply` requires. Line-level hunks honor one shared [`DiffOptions`](rustdoc:grit_lib::diff::DiffOptions) value (algorithm, context, indent heuristic, renames, optional word-level inline emphasis) whether you emit unified patch text or structured hunks for the CLI. Defaults match Git (`Myers`, `-U3`, indent heuristic on, rename similarity 50). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus structured or unified bodies; [`grit diff --patch-text`](../../commands/diff.md) calls `write_patch` for revisions and `write_patch_resolved` for staged and work-tree changes.

## Diffstat

[`DiffStat`](rustdoc:grit_lib::diff::DiffStat) is the data behind `--stat`, `--numstat` and `--shortstat`. [`Diff::stat`](rustdoc:grit_lib::diff::Diff::stat) builds one for a list of entries. [`DiffStat::from_files`](rustdoc:grit_lib::diff::DiffStat::from_files) takes the rows from `Diff::files_for_entries` at any detail level: each row's counts come from [`FileDiff::line_stat`](rustdoc:grit_lib::diff::ListedFileDiff::line_stat), which uses the stat when one was computed and otherwise counts the structured or combined hunk lines, so a patch-mode listing and a stat-mode listing give the same diffstat. [`totals`](rustdoc:grit_lib::diff::DiffStat::totals) sums the rows. To draw bars, scale each row with [`LineStat::bar`](rustdoc:grit_lib::diff::LineStat::bar) against [`max_changes`](rustdoc:grit_lib::diff::DiffStat::max_changes) and a column budget; the returned [`StatBar`](rustdoc:grit_lib::diff::StatBar) gives `+` and `-` column counts. `grit diff --stat`, `grit show`, `grit log --stat` and `grit stash show` render this model.

```rust
use grit_lib::diff::{DiffOptions, TreeDiffOptions};

let entries = repo
    .diff()
    .tree_to_tree(Some(&old_tree), Some(&new_tree), &TreeDiffOptions::default())?
    .entries;
// `DiffOptions::default()` pairs renames at Git's -M50 before counting lines. When the
// entries are already rename-aware, pass `DiffOptions::default().with_renames(RenameDetection::Off)`.
let stat = repo.diff().stat(entries, &DiffOptions::default())?;
for file in stat.files() {
    let bar = file.lines.bar(stat.max_changes(), 40);
    println!("{} | {}{}", file.path, "+".repeat(bar.plus), "-".repeat(bar.minus));
}
let totals = stat.totals();
println!("{} files, +{} -{}", totals.files_changed, totals.insertions, totals.deletions);
```

Under any whitespace rule (`-w`, `-b`, `--ignore-space-at-eol`, `--ignore-blank-lines`), Git leaves out of `--stat` any modified text file whose changes were all ignored. [`FileDiff::changes_all_ignored`](rustdoc:grit_lib::diff::ListedFileDiff::changes_all_ignored) tells you which rows those are. Additions, deletions, renames, mode changes and binary files stay listed. For a stash, [`RepositoryStash::diff_stat`](rustdoc:grit_lib::repo_handles::RepositoryStash::diff_stat) returns the finished `DiffStat` of `stash@{n}` against its base (`git stash show --stat`). It counts lines without building hunks and applies that whitespace rule.

For history listings, [`Diff::commit_files_many`](rustdoc:grit_lib::diff::Diff::commit_files_many) diffs a batch of commits against their first parents in parallel, reusing each parent tree when the commits are newest-first linear history; `grit log --stat`, `--numstat` and `-p` use it. When you only need per-commit counts, [`Diff::commit_line_totals`](rustdoc:grit_lib::diff::Diff::commit_line_totals) returns one [`DiffTotals`](rustdoc:grit_lib::diff::DiffTotals) per commit (the numbers in `git log --shortstat`), limited to the [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) you pass. Merges are not diffed and get `None`; a root commit is compared with the empty tree. A terminal `grit log` draws these as the `+added -removed` column and hides commits whose totals are zero.

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

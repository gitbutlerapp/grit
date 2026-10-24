---
title: Index
summary: Load the staging index, stage paths with porcelain add, and write a tree object from staged entries.
---

The Git index (staging area) lives in `Index`. Open a `Repository`, then use `IndexHandle` via `Repository::index_handle` (`load` / `write`). Index extensions (sparse checkout, split index, untracked cache) live under the [`index`](rustdoc:grit_lib::index) domain; working-tree helpers are under [`worktree`](rustdoc:grit_lib::worktree). See [API layout](../api-layout/).

See also the [Repository](../repository/) and [Objects](../objects/) pages for opening repos and writing objects. Other library topics: [Refs](../refs/), [Diff](../diff/).

## Reading the index

`IndexHandle::load` reads `index` from disk with the same sparse-checkout and split-index rules porcelain commands use. Entries are `IndexEntry` values with path, mode, object id, and stage.

## Staging paths

`porcelain::add::stage` compares the index to the work tree once, hashes changed blobs, and writes the index back. Pass `StageOptions` for pathspecs and `StageMode` (`All` vs `Update`). Long-running staging reports through a `ProgressSink`; examples use `NullProgress` when no UI is needed.

The `grit add` command is a thin wrapper around this API.

`StageOptions::pathspecs` are work-tree-relative. To turn command-line arguments into that form, use [`ResolvedPathspecs::resolve`](rustdoc:grit_lib::pathspec::ResolvedPathspecs) for a whole argument list (plus `--pathspec-from-file` input), or [`Repository::resolve_worktree_pathspec`](rustdoc:grit_lib::repo::Repository::resolve_worktree_pathspec) for one argument. Both apply the current-directory prefix and, on filesystems that alias NFD/NFC spellings (macOS with `core.precomposeunicode`), NFC-normalize the path. That decision is cached on the `Repository`, so passing hundreds of literal paths costs no config reload per path, and literal pathspecs are matched against the index with hash lookups on every platform.

## Worktree file modes

Some checkouts cannot show every mode Git records. With `core.filemode=false` the executable bit on disk means nothing, and with `core.symlinks=false` (the Windows default when symlinks need admin rights) a symlink is checked out as a plain file holding the link target. [`WorktreeModePolicy`](rustdoc:grit_lib::index::WorktreeModePolicy) reads both settings with `from_config` and maps an `lstat` mode onto the mode already in the index: `index_mode_for_worktree` keeps a recorded `120000` for a plain file when symlinks are disabled, and keeps a recorded `100644`/`100755` when the filemode is not trusted. Staging, `commit`, `status` and worktree diffs all go through it, so an untouched link stays a link and an edited one is staged as a new link target, the same as Git.

## Writing a tree from the index

[`write_tree_from_index`](rustdoc:grit_lib::index::write_tree::write_tree_from_index) builds a tree object from stage-0 entries (respecting the optional path prefix). It returns the root [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). When the index cache-tree extension is valid, grit reuses cached subtree oids for speed; otherwise it walks entries and writes new tree objects through [`Odb`](rustdoc:grit_lib::objects::odb::Odb).

## Example

This program loads the index, stages new or changed paths, and prints the tree oid grit would commit:

<!-- include: grit-examples/src/bin/guide_index.rs -->

With a repository path, compare the printed tree to Git:

```bash
cargo run --bin guide_index /path/to/repo
git -C /path/to/repo write-tree
git -C /path/to/repo fsck --strict
```

Without arguments the binary uses a temporary repository (useful for `cargo run`, not for fsck demos).

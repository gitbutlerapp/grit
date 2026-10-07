---
title: Index
summary: Load the staging index, stage paths with porcelain add, and write a tree object from staged entries.
---

The Git index (staging area) lives in [`Index`](rustdoc:grit_lib::index::Index). Open a [`Repository`](rustdoc:grit_lib::repo::Repository), then call `load_index` (or `write_index` after changes). Index-related modules are grouped under [`worktree_index`](rustdoc:grit_lib::worktree_index) in rustdoc.

See also the [Repository](repository/) and [Objects](objects/) pages for opening repos and writing objects. Other library topics: [Refs](refs/), [Diff](diff/).

## Reading the index

[`Index`](rustdoc:grit_lib::index::Index) `load` reads `index` from disk; [`Repository`](rustdoc:grit_lib::repo::Repository) `load_index` applies sparse-checkout and split-index rules the same way porcelain commands do. Entries are [`IndexEntry`](rustdoc:grit_lib::index::IndexEntry) values with path, mode, object id, and stage.

## Staging paths

[`porcelain::add::stage`](rustdoc:grit_lib::porcelain::add::stage) compares the index to the work tree once, hashes changed blobs, and writes the index back. Pass [`StageOptions`](rustdoc:grit_lib::porcelain::add::StageOptions) for pathspecs and [`StageMode`](rustdoc:grit_lib::porcelain::add::StageMode) (`All` vs `Update`). Long-running staging reports through a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink); examples use [`NullProgress`](rustdoc:grit_lib::progress::NullProgress) when no UI is needed.

The `grit add` command is a thin wrapper around this API.

## Writing a tree from the index

[`write_tree_from_index`](rustdoc:grit_lib::write_tree::write_tree_from_index) builds a tree object from stage-0 entries (respecting the optional path prefix). It returns the root [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). When the index cache-tree extension is valid, grit reuses cached subtree oids for speed; otherwise it walks entries and writes new tree objects through [`Odb`](rustdoc:grit_lib::odb::Odb).

## Example

This program loads the index, stages new or changed paths, and prints the tree oid grit would commit:

<!-- include: grit-examples/src/bin/guide_index.rs -->

With a repository path, compare the printed tree to Git:

```
cargo run --bin guide_index /path/to/repo
git -C /path/to/repo write-tree
git -C /path/to/repo fsck --strict
```

Without arguments the binary uses a temporary repository (useful for `cargo run`, not for fsck demos).

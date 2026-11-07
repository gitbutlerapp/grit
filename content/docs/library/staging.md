---
title: Index
summary: Load the staging index, stage paths with porcelain add, and write a tree object from staged entries.
---

The Git index (staging area) lives in `Index`. Open a `Repository`, then use `IndexFile` via `Repository::index` (`load` / `write`). Index extensions (sparse checkout, split index, untracked cache) live under the [`index`](rustdoc:grit_lib::index) domain; working-tree helpers are under [`worktree`](rustdoc:grit_lib::worktree). See [API layout](../api-layout/).

See also the [Repository](../repository/) and [Objects](../objects/) pages for opening repos and writing objects. Other library topics: [Refs](../refs/), [Diff](../diff/).

## Reading the index

[`IndexFile::load`](rustdoc:grit_lib::index::IndexFile::load) reads the index (`GIT_INDEX_FILE` or `.git/index`), resolves a split index, and by default expands every sparse-directory placeholder (full in-memory materialization). [`IndexLoadOptions`](rustdoc:grit_lib::index::IndexLoadOptions) selects another file (`with_path`), keeps placeholders for read-mostly work such as `status` and `diff` on cone sparse-index checkouts (`with_sparse(SparseIndexLoad::Preserve)`), or sets the entry-loading thread policy (`with_parse_context`). Entries are `IndexEntry` values with path, mode, object id, and stage.

```rust
use grit_lib::index::{IndexLoadOptions, IndexWriteOptions, SparseIndexLoad};

let mut index = repo.index().load(&IndexLoadOptions::default())?;
// ... edit entries ...
repo.index().write(&mut index, &IndexWriteOptions::default())?;

let compact = repo
    .index()
    .load(&IndexLoadOptions::default().with_sparse(SparseIndexLoad::Preserve))?;
```

[`IndexFile::write`](rustdoc:grit_lib::index::IndexFile::write) collapses skip-worktree subtrees back into sparse-directory entries when `index.sparse` is on, writes a split index when [`IndexWriteOptions::split`](rustdoc:grit_lib::index::IndexWriteOptions) or `core.splitIndex` asks for one, and runs the `post-index-change` hook. To parse index bytes you already have, use [`Index::parse`](rustdoc:grit_lib::index::Index::parse) with [`IndexParseOptions`](rustdoc:grit_lib::index::IndexParseOptions) (hash algorithm, config for EOIE/IEOT, source path for errors, thread policy); [`Index::empty_for`](rustdoc:grit_lib::index::Index::empty_for) makes an empty index whose version follows `index.version` and `feature.manyFiles`.

## Staging paths

`porcelain::add::stage` compares the index to the work tree once, hashes changed blobs, and writes the index back. Pass `StageOptions` for pathspecs and `StageMode` (`All` vs `Update`). Long-running staging reports through a `ProgressSink`; examples use `NullProgress` when no UI is needed.

The `grit add` command is a thin wrapper around this API.

`StageOptions::pathspecs` are work-tree-relative. Prefer [`Repository::pathspec`](rustdoc:grit_lib::repo::Repository::pathspec) with [`PathspecOptions`](rustdoc:grit_lib::worktree::pathspec::PathspecOptions) to resolve argv and `--pathspec-from-file` input in one step ([`Pathspec::specs`](rustdoc:grit_lib::worktree::pathspec::Pathspec::specs) for matching, [`Pathspec::iter`](rustdoc:grit_lib::worktree::pathspec::Pathspec::iter) for display). The resolved [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) carries both display sources and normalized specs ([`Pathspec::iter`](rustdoc:grit_lib::worktree::pathspec::Pathspec::iter)). Resolution applies the current-directory prefix and, on filesystems that alias NFD/NFC spellings (macOS with `core.precomposeunicode`), NFC-normalizes paths. That decision is cached on the `Repository`, so passing hundreds of literal paths costs no config reload per path, and literal pathspecs are matched against the index with hash lookups on every platform.

[`StatusOptions::pathspec`](rustdoc:grit_lib::porcelain::status::StatusOptions::pathspec) uses the same [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) type; `grit status` resolves CLI path arguments through [`Repository::pathspec`](rustdoc:grit_lib::repo::Repository::pathspec).

## Worktree file modes

Some checkouts cannot show every mode Git records. With `core.filemode=false` the executable bit on disk means nothing, and with `core.symlinks=false` (the Windows default when symlinks need admin rights) a symlink is checked out as a plain file holding the link target. [`WorktreeModePolicy`](rustdoc:grit_lib::index::WorktreeModePolicy) reads both settings with `from_config` and maps an `lstat` mode onto the mode already in the index: `index_mode_for_worktree` keeps a recorded `120000` for a plain file when symlinks are disabled, and keeps a recorded `100644`/`100755` when the filemode is not trusted. Staging, `commit`, `status` and worktree diffs all go through it, so an untouched link stays a link and an edited one is staged as a new link target, the same as Git.

## Sparse checkout

[`SparseCheckout::load`](rustdoc:grit_lib::index::sparse_checkout::SparseCheckout::load) compiles the repository's sparse-checkout definition (`core.sparseCheckout`, `core.sparseCheckoutCone` and `info/sparse-checkout`, cone or non-cone) once, and returns `None` when sparse checkout is off. [`SparseCheckout::includes`](rustdoc:grit_lib::index::sparse_checkout::SparseCheckout::includes) answers whether a path belongs in the work tree.

[`IndexFile::apply_sparse_checkout`](rustdoc:grit_lib::index::IndexFile::apply_sparse_checkout) marks the excluded stage-0 entries of an index built from a tree as skip-worktree, as Git does after `read-tree` or a branch checkout.

Every operation that updates the work tree from a tree consults it: `reset --hard` and `--mixed`, `switch`, `merge`, `pull` (merge and rebase), stash and the history engine. A path outside the definition that is skip-worktree (or new to the index) gets an index row with the `skip-worktree` bit and nothing is written to disk, the same as `git`. Conflicted paths are written out with conflict markers, also like `git`. When `index.sparse` is on, the index is collapsed back into sparse-directory entries when it is written. [`SparseCheckout::carry_skip_worktree`](rustdoc:grit_lib::index::sparse_checkout::SparseCheckout::carry_skip_worktree) keeps the bits on an index rebuilt from a tree without touching the work tree.

## Writing a tree from the index

[`write_tree_from_index`](rustdoc:grit_lib::index::write_tree::write_tree_from_index) builds a tree object from stage-0 entries (respecting the optional path prefix). It returns the root [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). When the index cache-tree extension is valid, grit reuses cached subtree oids for speed; otherwise it walks entries and writes new tree objects through [`Odb`](rustdoc:grit_lib::objects::odb::Odb).

## Errors

Index load, write, extension parsing, and write-tree failures return [`IndexError`](rustdoc:grit_lib::index::IndexError) (wrapped as [`Error::Index`](rustdoc:grit_lib::error::Error::Index) at the repository boundary). Match on the enum, not message text:

| Variant | When |
| --- | --- |
| `Corrupt` | Bad header, unsupported version, truncated bytes, checksum mismatch, or malformed entries/extensions (`IndexCorruptReason` pinpoints the site). |
| `Unmerged` | Conflict stages remain; commit or write-tree needs a clean stage-0 index. |
| `PathPrefixConflict` | Two paths disagree on whether a prefix is a file or directory. |
| `FsmonitorExtension*` | The `FSMN` extension is truncated, has an unknown version, or carries an invalid EWAH bitmap. |
| `CacheTreeCorrupt` | Cache-tree metadata does not match index entries. |
| `LockHeld` | Another process holds `index.lock` (also surfaced as top-level [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld)). |
| `PathIo` | Filesystem I/O on the index path. |
| `SplitIndex` | Split-index link/read/write failed. |
| `Chmod` | Mode change rejected (unmerged path, non-regular file, invalid mode). |
| `WriteTree` | Structured write-tree / cache-tree rebuild failure. |

Stable JSON `kind` slugs (`index_error`, `index_unmerged`, `cache_tree_corrupt`, …) come from [`IndexError::kind_name`](rustdoc:grit_lib::index::IndexError::kind_name). See the [errors](../errors/) overview for the cross-area map.

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

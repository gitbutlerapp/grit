# Index

> Load the staging index, stage paths with porcelain add, and write a tree object from staged entries.

The Git index (staging area) lives in [`Index`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html). Open a [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html), then call `load_index` (or `write_index` after changes). Index-related modules are grouped under [`worktree_index`](https://docs.rs/grit-lib/latest/grit_lib/worktree_index/index.html) in rustdoc.

See also the [Repository](https://grit-scm.com/docs/library/repository/index.md) and [Objects](https://grit-scm.com/docs/library/objects/index.md) pages for opening repos and writing objects. Other library topics: [Refs](https://grit-scm.com/docs/library/refs/index.md), [Diff](https://grit-scm.com/docs/library/diff/index.md).

## Reading the index

[`Index`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html) `load` reads `index` from disk; [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) `load_index` applies sparse-checkout and split-index rules the same way porcelain commands do. Entries are [`IndexEntry`](https://docs.rs/grit-lib/latest/grit_lib/index/struct.IndexEntry.html) values with path, mode, object id, and stage.

## Staging paths

[`porcelain::add::stage`](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/fn.stage.html) compares the index to the work tree once, hashes changed blobs, and writes the index back. Pass [`StageOptions`](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/struct.StageOptions.html) for pathspecs and [`StageMode`](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/enum.StageMode.html) (`All` vs `Update`). Long-running staging reports through a [`ProgressSink`](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.ProgressSink.html); examples use [`NullProgress`](https://docs.rs/grit-lib/latest/grit_lib/progress/struct.NullProgress.html) when no UI is needed.

The `grit add` command is a thin wrapper around this API.

## Writing a tree from the index

[`write_tree_from_index`](https://docs.rs/grit-lib/latest/grit_lib/write_tree/fn.write_tree_from_index.html) builds a tree object from stage-0 entries (respecting the optional path prefix). It returns the root [`ObjectId`](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectId.html). When the index cache-tree extension is valid, grit reuses cached subtree oids for speed; otherwise it walks entries and writes new tree objects through [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html).

## Example

This program loads the index, stages new or changed paths, and prints the tree oid grit would commit:

```rust
//! Read the index, stage paths, and write a tree from staged entries.
//!
//! Source for the library guide "Index" page (included in the docs site).

use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::write_tree::write_tree_from_index;
use std::fs;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    Repository::open(&git_dir, work_tree)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let mut temp_guard = None;
    let repo = if let Some(root) = std::env::args().nth(1).map(PathBuf::from) {
        open_repo(&root)?
    } else {
        let temp = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
        init_repository(
            temp.path(),
            false,
            "main",
            None,
            grit_lib::RefStorageFormat::Files,
        )?;
        let path = temp.path().join("hello.txt");
        fs::write(&path, b"staged from the library guide\n").map_err(grit_lib::error::Error::Io)?;
        let opened = open_repo(temp.path())?;
        temp_guard = Some(temp);
        opened
    };
    let _keep = temp_guard;

    let index = repo.load_index()?;
    println!("index_entries={}", index.entries().len());

    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress)?;
    println!(
        "staged added={} modified={} removed={}",
        outcome.added, outcome.modified, outcome.removed
    );

    let index = repo.load_index()?;
    let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    println!("tree_oid={}", tree_oid.to_hex());

    Ok(())
}
```

With a repository path, compare the printed tree to Git:

```bash
cargo run --bin guide_index /path/to/repo
git -C /path/to/repo write-tree
git -C /path/to/repo fsck --strict
```

Without arguments the binary uses a temporary repository (useful for `cargo run`, not for fsck demos).

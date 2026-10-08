# Objects

> ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.

Git stores four object kinds grit-lib exposes as [`ObjectKind`](https://docs.rs/grit-lib/latest/grit_lib/objects/enum.ObjectKind.html). Every object is named by an [`ObjectId`](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectId.html) (SHA-1 by default). The [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) on [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) reads loose objects and packed storage transparently.

## Writing

[`Odb::write`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree and commit bodies must already be in Git’s text format; use [`parse_tree`](https://docs.rs/grit-lib/latest/grit_lib/objects/fn.parse_tree.html) and [`parse_commit`](https://docs.rs/grit-lib/latest/grit_lib/objects/fn.parse_commit.html) when reading them back.

[`Odb::hash`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) (or [`HashAlgo::hash_object`](https://docs.rs/grit-lib/latest/grit_lib/objects/enum.HashAlgo.html)) computes an id without writing—useful for dry runs and tests. The algorithm follows the repository’s configured object format (SHA-1 or SHA-256).

## Reading

[`Odb::read`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) returns an [`Object`](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.Object.html) with `kind` and uncompressed `data`. Packed and loose objects share the same API.

When you only need type and size (for example listing objects without loading blob bodies), use [`Odb::read_info`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) (see `read_info` on [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html)). It returns [`ObjectInfo`](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectInfo.html) and avoids inflating full payloads for loose objects and non-delta pack entries; delta chains are resolved from headers and delta size varints only.

## Pack read caching

[`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) owns a repository-scoped [`PackStore`](https://docs.rs/grit-lib/latest/grit_lib/pack_store/struct.PackStore.html): pack directory listings, parsed `.idx` files, pack bytes, MIDX layers, and the delta-base LRU. Cloned [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) handles share the same store; alternate object directories get separate stores on the parent [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html).

After repack, garbage collection, or installing a pack with [`install_pack_bytes`](https://docs.rs/grit-lib/latest/grit_lib/index_pack/fn.install_pack_bytes.html), call [`Odb::invalidate_packs`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) so the next read rescans `objects/pack/`. If another [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) in the same process still holds a stale listing, a lookup miss retriggers directory reprepare when the pack folder’s mtime changes.

## Example

This example initializes a repository, writes a blob, tree, and commit, verifies structure in memory, and prints the commit id:

```rust
//! Write a blob, tree, and commit through [`grit_lib::odb::Odb`], then read them back.
//!
//! Source for the library guide "Objects" page (included in the docs site).

use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, serialize_tree, CommitData, ObjectKind, TreeEntry,
};
use grit_lib::repo::{init_repository, Repository};
use std::path::{Path, PathBuf};

fn write_demo_objects(
    repo: &Repository,
) -> Result<
    (
        grit_lib::objects::ObjectId,
        grit_lib::objects::ObjectId,
        grit_lib::objects::ObjectId,
    ),
    grit_lib::error::Error,
> {
    let blob_data = b"hello from the library guide\n";
    let blob_oid = repo.odb.write(ObjectKind::Blob, blob_data)?;

    let tree_entries = vec![TreeEntry {
        mode: 0o100644,
        name: b"README".to_vec(),
        oid: blob_oid,
    }];
    let tree_oid = repo
        .odb
        .write(ObjectKind::Tree, &serialize_tree(&tree_entries))?;

    let commit = CommitData {
        tree: tree_oid,
        parents: Vec::new(),
        author: "Ada Lovelace <ada@example.com> 0 +0000".to_owned(),
        committer: "Ada Lovelace <ada@example.com> 0 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "Library guide objects example\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let commit_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;

    let tree_obj = repo.odb.read(&tree_oid)?;
    let entries = parse_tree(&tree_obj.data)?;
    assert_eq!(entries[0].oid, blob_oid);

    let commit_obj = repo.odb.read(&commit_oid)?;
    let parsed = parse_commit(&commit_obj.data)?;
    assert_eq!(parsed.tree, tree_oid);

    Ok((blob_oid, tree_oid, commit_oid))
}

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
    let repo = if let Some(root) = std::env::args().nth(1).map(PathBuf::from) {
        open_repo(&root)?
    } else {
        let temp = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
        init_repository(temp.path(), false, "main", None, "files")?;
        open_repo(temp.path())?
    };

    let (blob_oid, tree_oid, commit_oid) = write_demo_objects(&repo)?;
    println!("{commit_oid}");
    println!("blob={blob_oid} tree={tree_oid}");
    Ok(())
}
```

Objects land on disk in standard loose format. System Git can read them:

```bash
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).

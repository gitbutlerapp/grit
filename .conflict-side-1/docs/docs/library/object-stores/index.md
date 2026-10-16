# Object stores

> Pluggable ObjectStore backends, OdbBuilder, and the odb_conformance test suite.

By default [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) opens a files-backed [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) (loose objects, packs, and optional MIDX). Embedders can swap the **primary** store or add read-only layers with [`OdbBuilder`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.OdbBuilder.html) and `Repository::open_with_odb`.

## Traits

| Trait | Role |
| ----- | ---- |
| [`ObjectStore`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.ObjectStore.html) | Read, metadata, streaming, enumeration |
| [`WritableObjectStore`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.WritableObjectStore.html) | Insert objects (idempotent writes) |

Built-in backends live under [`grit_lib::odb::store`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/index.html) (`LooseStore`, `FilesSource`, `MemoryStore`, `CompositeStore`, and others).

## Lookup order

For a given [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) handle:

1. In-memory overlay (when enabled for merge-tree-style operations)
2. **Primary** store — files source by default, or your custom [`WritableObjectStore`](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.WritableObjectStore.html)
3. Extra read sources from `OdbBuilder::push_read_source`
4. Alternate object directories when `OdbBuilder::alternates` is true (`info/alternates`, environment alternates, submodule object dirs)

## Building a custom repository

```rust
use std::sync::Arc;

use grit_lib::environment::RepositoryOptions;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::MemoryStore;
use grit_lib::odb::OdbBuilder;
use grit_lib::repo::{init_repository, Repository};

let dir = tempfile::tempdir()?;
init_repository(dir.path(), false, "main", None, "files")?;
let git_dir = dir.path().join(".git");
let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
let repo = Repository::open_with_odb(
    &RepositoryOptions::empty(),
    &git_dir,
    Some(dir.path()),
    OdbBuilder::files(git_dir.join("objects"))
        .primary(store)
        .alternates(false),
)?;
let _blob = repo.odb.write(ObjectKind::Blob, b"hello")?;
# Ok::<(), grit_lib::error::Error>(())
```

Filesystem-only maintenance (`Odb::gc`, `Odb::write_commit_graph`, `Odb::pack_store`, pack install, MIDX write) returns [`Error::UnsupportedObjectStore`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) when the primary is not the default files backend. Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites. Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.OdbBuilder.html).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

```rust
//! Custom append-only object store wired through [`OdbBuilder::primary`].
//!
//! Writes a small commit graph in the KV store, walks history with [`rev_list`],
//! then exports objects as loose files for interoperability with system Git.

use std::env;
use std::path::PathBuf;

use grit_examples::packfile_kv;

fn main() -> grit_lib::error::Result<()> {
    let (root, _tmpdir) = match env::args().nth(1) {
        Some(path) => (PathBuf::from(path), None),
        None => {
            let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
            let root = dir.path().to_path_buf();
            (root, Some(dir))
        }
    };
    let log = packfile_kv::run_custom_object_store_demo(&root)?;
    for oid in log {
        println!("{oid}");
    }
    Ok(())
}
```

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

```rust
//! SQLite object store wired through [`OdbBuilder::primary`].
//!
//! Writes a small commit graph in SQLite, walks history with [`rev_list`],
//! then exports objects as loose files for interoperability with system Git.

use std::env;
use std::path::PathBuf;

use grit_examples::sqlite_odb;

fn main() -> grit_lib::error::Result<()> {
    let (root, _tmpdir) = match env::args().nth(1) {
        Some(path) => (PathBuf::from(path), None),
        None => {
            let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
            let root = dir.path().to_path_buf();
            (root, Some(dir))
        }
    };
    let log = sqlite_odb::run_sqlite_object_store_demo(&root)?;
    for oid in log {
        println!("{oid}");
    }
    Ok(())
}
```

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

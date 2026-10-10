---
title: Object stores
summary: Pluggable ObjectStore backends, OdbBuilder, and the odb_conformance test suite.
---

By default [`Repository`](rustdoc:grit_lib::repo::Repository) opens a files-backed [`Odb`](rustdoc:grit_lib::odb::Odb) (loose objects, packs, and optional MIDX). Embedders can swap the **primary** store or add read-only layers with [`OdbBuilder`](rustdoc:grit_lib::odb::OdbBuilder) and `Repository::open_with_odb`.

## Traits

| Trait | Role |
| ----- | ---- |
| [`ObjectStore`](rustdoc:grit_lib::odb::store::ObjectStore) | Read, metadata, streaming, enumeration |
| [`WritableObjectStore`](rustdoc:grit_lib::odb::store::WritableObjectStore) | Insert objects (idempotent writes) |

Built-in backends live under [`grit_lib::odb::store`](rustdoc:grit_lib::odb::store) (`LooseStore`, `FilesSource`, `MemoryStore`, `CompositeStore`, and others).

## Lookup order

For a given [`Odb`](rustdoc:grit_lib::odb::Odb) handle:

1. In-memory overlay (when enabled for merge-tree-style operations)
2. **Primary** store — files source by default, or your custom [`WritableObjectStore`](rustdoc:grit_lib::odb::store::WritableObjectStore)
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

Filesystem-only maintenance (`Odb::gc`, `Odb::write_commit_graph`, `Odb::pack_store`, pack install, MIDX write) returns [`Error::UnsupportedObjectStore`](rustdoc:grit_lib::error::Error) when the primary is not the default files backend. Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites. Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

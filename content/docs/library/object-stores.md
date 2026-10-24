---
title: Object stores
summary: Pluggable ObjectStore backends, OdbBuilder, and the odb_conformance test suite.
---

By default [`Repository`](rustdoc:grit_lib::repo::Repository) opens a files-backed [`Odb`](rustdoc:grit_lib::objects::odb::Odb) (loose objects, packs, and optional MIDX). Embedders can swap the **primary** store or add read-only layers with [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) and [`RepositoryBuilder`](rustdoc:grit_lib::repo::RepositoryBuilder).

## Traits

| Trait | Role |
| ----- | ---- |
| [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) | Read, metadata, streaming, enumeration |
| [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore) | Insert objects (idempotent writes) |

Built-in backends live under [`grit_lib::odb::store`](rustdoc:grit_lib::objects::odb::store) (`LooseStore`, `FilesSource`, `MemoryStore`, `CompositeStore`, and others).

## Lookup order

For a given [`Odb`](rustdoc:grit_lib::objects::odb::Odb) handle:

1. In-memory overlay (when enabled for merge-tree-style operations)
2. **Primary** store — files source by default, or your custom [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore)
3. Extra read sources from `OdbBuilder::push_read_source`
4. Alternate object directories when `OdbBuilder::alternates` is true (`info/alternates`, environment alternates, submodule object dirs)

## Building a custom repository

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

let dir = tempfile::tempdir()?;
Repository::init(dir.path(), &InitOptions::default())?;
let git_dir = dir.path().join(".git");
let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
let repo = RepositoryBuilder::new(RepositoryOptions::empty())
    .odb_builder(
        OdbBuilder::files(git_dir.join("objects"))
            .primary(store)
            .alternates(false),
    )
    .open(&git_dir, Some(dir.path()))?;
let _blob = repo.odb.write(ObjectKind::Blob, b"hello")?;
# Ok::<(), grit_lib::error::Error>(())
```

Every object a porcelain operation creates goes to the primary store through [`WritableObjectStore::write`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore::write): staging new and modified files ([`porcelain::add::stage`](rustdoc:grit_lib::porcelain::add::stage), [`stage_for_commit`](rustdoc:grit_lib::porcelain::add::stage_for_commit)), [`commit_worktree`](rustdoc:grit_lib::porcelain::commit::commit_worktree), stash, notes and annotated tags. Nothing is written under `objects/`. [`Repository::fsck`](rustdoc:grit_lib::repo::Repository::fsck) validates a custom primary by listing its objects with `ObjectStore::for_each_object`, re-hashing each one against its id and running the usual object checks; read or hash failures are reported as `FsckIssue::BadObject` because there is no loose file path to name.

Filesystem-only maintenance returns `Error::UnsupportedObjectStore` when the primary is not the default files backend: `Odb::gc` and prune, repack, `Odb::write_commit_graph`, MIDX write, `Odb::pack_store` and pack install (index-pack), and `Odb::write_loose_zlib_prehashed`. Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites. Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

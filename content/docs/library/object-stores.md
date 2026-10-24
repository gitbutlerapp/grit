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

Pass an [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) rooted at the repository's `objects/` directory to [`RepositoryBuilder::odb_builder`](rustdoc:grit_lib::repo::RepositoryBuilder::odb_builder). Any path naming that directory works: one through symlinks (such as macOS `/var` → `/private/var` or `/tmp` → `/private/tmp`) or one relative to the environment's current directory. The opened [`Odb`](rustdoc:grit_lib::objects::odb::Odb) reports the canonical path. A builder rooted anywhere else fails with [`Error::OdbObjectsDirMismatch`](rustdoc:grit_lib::error::Error::OdbObjectsDirMismatch).

<!-- include: grit-examples/src/bin/memory_primary.rs -->

```bash
cargo run -p grit-examples --bin memory_primary
```

Filesystem-only maintenance (`Odb::gc`, `Odb::write_commit_graph`, `Odb::pack_store`, pack install, MIDX write) returns `Error::UnsupportedObjectStore` when the primary is not the default files backend. Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites. Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

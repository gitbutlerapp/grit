---
title: Writing storage backends
summary: Implement object and ref stores, open a repository on them, test with conformance suites, and know what still needs a .git directory on disk.
---

Embedders replace grit’s default files layout with their own **object store**, **ref store**, or both. This page is the end-to-end guide: traits to implement, how to wire [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions), optional capabilities, performance hooks, and how to prove correctness with the shared conformance suites. For lookup order, alternates, pack ingest, and benchmark tables on custom primaries, see [Object stores](object-stores.md).

## What to implement

### Object storage (read path)

[`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) is the minimum surface:

| Method | Contract |
| ------ | -------- |
| `read` / `read_info` / `open_stream` | Return `Ok(None)` when the id is absent; propagate I/O or corruption errors (do not map outages to “missing”). |
| `contains` | Must match what `read` would return for the same id. |
| `hash_algo` | Must match `extensions.objectformat` / repository init. |
| `for_each_object` | List every stored id; release internal locks before calling the callback (callbacks often call back into the store). |
| `lookup_prefix` / `contains_many` | Override when a full scan per call is too expensive (see [Performance](#performance-checklist)). |

Optional streaming helpers: [`ObjectStore::read_into`](rustdoc:grit_lib::objects::odb::store::ObjectStore::read_into) (default copies from `read`), [`ObjectStore::with_stream`](rustdoc:grit_lib::objects::odb::Odb::with_stream) on the repository `Odb` handle for large blobs.

### Object storage (write path)

[`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore): idempotent `write`, plus `flush` / `rollback` when you batch transactions. Pack fetch, push, bundle install, and commits all end in `flush` on the primary.

### Pruning (optional)

[`PrunableObjectStore`](rustdoc:grit_lib::objects::odb::store::PrunableObjectStore): implement `remove`, `as_prunable`, and optionally `object_mtime` for grace-period prune. Without it, `gc`, `prune`, and `Odb::gc` return [`RepositoryError::UnsupportedObjectStore`](rustdoc:grit_lib::repo::RepositoryError::UnsupportedObjectStore) with operation `prune` — never touch `objects/` on disk silently.

### Ref storage

[`RefStore`](rustdoc:grit_lib::refs::store::RefStore): `read_raw`, `resolve`, iteration (`for_each_ref` with the empty-prefix rule from [`RefStore`](rustdoc:grit_lib::refs::store::RefStore)), reflog read/write, and **transactions** via `prepare` → `commit` / `abort`.

You do not reimplement Git’s ref rules: use [`refs::store::backend`](rustdoc:grit_lib::refs::store::backend) helpers (`check_transaction`, `apply_transaction`, `RefBatchState`, `SingleWriterGate` for in-process writers). Report backend failures as [`RefStoreError::Backend`](rustdoc:grit_lib::refs::store::RefStoreError::Backend).

## Opening and initializing

### Inject stores when opening

[`RepositoryOptions::with_object_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_object_store) is the only supported way to set a custom primary object store. Add read layers, an attached commit-graph, or alternates with [`RepositoryOptions::with_odb_builder`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_odb_builder) ([`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder)).

[`RepositoryOptions::with_ref_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_ref_store) replaces loose/packed/reftable refs under `refs/` and the on-disk `HEAD` file. Pair it with [`with_initial_branch`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_initial_branch) when `HEAD` should start on an unborn branch.

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::HashAlgo;
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{InitOptions, Repository};

let dir = tempfile::tempdir()?;
let (repo, _) = Repository::init(
    dir.path(),
    &InitOptions::new()
        .with_initial_branch("main")
        .with_repository_options(
            RepositoryOptions::empty()
                .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .with_ref_store(Arc::new(MemoryRefStore::new()))
                .with_work_tree(dir.path()),
        ),
)?;
# Ok::<(), grit_lib::error::Error>(())
```

### What `Repository::init` writes

[`Repository::init`](rustdoc:grit_lib::repo::Repository::init) with [`InitOptions::with_repository_options`](rustdoc:grit_lib::repo::InitOptions::with_repository_options) creates only what your stores do **not** own:

| Injected | On disk under the git directory |
| -------- | -------------------------------- |
| Object store only | Everything except `objects/` |
| Ref store only | Everything except `HEAD` and `refs/` |
| Both | `config` only (plus the work tree beside it when not bare) |

Object format comes from the store’s `hash_algo`; do not fight it with [`InitOptions::object_format`](rustdoc:grit_lib::repo::InitOptions::object_format) unless you mean to error with [`RepositoryError::ObjectFormatMismatch`](rustdoc:grit_lib::error::RepositoryError::ObjectFormatMismatch).

## Capability table

### `PrunableObjectStore`

| API | On a prunable custom primary |
| --- | --- |
| [`Maintenance::prune_loose_objects`](rustdoc:grit_lib::maintenance::Maintenance::prune_loose_objects) | Deletes unreachable objects past `gc.pruneExpire` |
| [`Maintenance::gc`](rustdoc:grit_lib::maintenance::Maintenance::gc) | Pack-refs + reflog expiry + prune (skips on-disk repack / commit-graph) |
| [`Odb::gc`](rustdoc:grit_lib::objects::odb::Odb::gc) | Deletes all unreachable objects (no grace) |

### `FilesOnlyOperation` (typed refusal)

Operations that need the on-disk `objects/` layout return [`RepositoryError::UnsupportedObjectStore`](rustdoc:grit_lib::repo::RepositoryError::UnsupportedObjectStore) with a [`FilesOnlyOperation`](rustdoc:grit_lib::repo::error::FilesOnlyOperation) variant — match on the enum, not strings. Grit’s fallbacks on a custom primary:

| Operation | Fallback on custom primary |
| --------- | -------------------------- |
| Repack / `.pack` install | Pack stream ingested object-by-object ([`ingest_pack_into`](rustdoc:grit_lib::objects::unpack_objects::ingest_pack_into), [`ingest_received_pack`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack)) |
| Commit-graph file | Build in memory with [`Maintenance::build_commit_graph`](rustdoc:grit_lib::maintenance::Maintenance::build_commit_graph); attach bytes via [`OdbBuilder::commit_graph`](rustdoc:grit_lib::objects::odb::OdbBuilder::commit_graph) |
| Promisor / partial clone ODB | Not supported (no silent filesystem promisor) |
| Prune / `gc` object deletion | Only with `PrunableObjectStore`; otherwise typed `prune` refusal |

The generated variant table lives on [Object stores](object-stores.md#what-works-with-a-custom-primary) (`<!-- grit:variant-table grit_lib::repo::error::FilesOnlyOperation -->`).

## Performance checklist

| Hook | Why |
| ---- | --- |
| [`read_into`](rustdoc:grit_lib::objects::odb::store::ObjectStore::read_into) | Rev-list, merge-base, and change-id scans reuse one buffer per commit instead of allocating every `read`. |
| [`contains_many`](rustdoc:grit_lib::objects::odb::store::ObjectStore::contains_many) | Write-tree and fetch/push negotiation ask about many ids at once; default is one `contains` per id. |
| [`lookup_prefix`](rustdoc:grit_lib::objects::odb::store::ObjectStore::lookup_prefix) | Short-id and `RevWalk::abbreviate`; default scans all objects. Use [`ObjectIdPrefix::lowest`](rustdoc:grit_lib::objects::ObjectIdPrefix::lowest) / [`highest`](rustdoc:grit_lib::objects::ObjectIdPrefix::highest) for sorted stores. |
| Attached commit-graph | Walks skip commit reads; see [Object stores — Commit-graphs for a custom store](object-stores.md#commit-graphs-for-a-custom-store) and the benchmark table under `<!-- grit:benchmark-tables -->` on [Benchmarks](../benchmarks.md) for rev-list / merge-base rows. |

## Testing

Three workspace conformance crates exercise backends before you wire a full `Repository`:

| Suite | Crate module | What it checks |
| ----- | ------------ | -------------- |
| Object read/write | `grit_test_support::odb_conformance` | `run_read_suite`, `run_write_suite` on `ObjectStore` / `WritableObjectStore` |
| Ref store | `grit_test_support::refstore_conformance` | `run_suite` on `Arc<dyn RefStore>` |
| Repository | `grit_test_support::repository_conformance` | `BackendFixture` + `run_repository_suite`, `run_parity` for typed `RepositorySnapshot` parity |

Example pattern (object store):

```rust
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};

#[test]
fn my_store_passes_odb_conformance() {
    run_read_suite(|| seed_my_store(&FIXTURE_OBJECTS));
    run_write_suite(|| MyStore::new(HashAlgo::Sha1));
}
```

End-to-end SQLite + in-memory refs (parity against the files backend):

- Source: [`grit-examples/tests/sqlite_repository_conformance.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/tests/sqlite_repository_conformance.rs)
- Library tests: [`grit-lib/tests/repository_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/repository_conformance_memory.rs), [`repository_conformance_files.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/repository_conformance_files.rs)

After changing integration tests under `grit-examples/tests/`, run `python3 scripts/sync_integration_tests.py`.

## What still needs a `.git` directory on disk

When **both** stores are injected, [`Repository::open`](rustdoc:grit_lib::repo::Repository::open) does not require a populated git directory; config can come from [`Environment::push_config_override`](rustdoc:grit_lib::config::environment::Environment::push_config_override) only. Even then, several subsystems still read or write paths under the git directory (or require a work tree):

| Area | Without on-disk git layout |
| ---- | --------------------------- |
| Objects, refs, config (injected) | Fully served by your stores + environment |
| Log, rev-parse, merge trees, notes, bundles to `Write` | Work through stores |
| Fetch (local) | Works; pack spool uses temp files; `FETCH_HEAD` not written |
| Index, staging, `COMMIT_EDITMSG`, merge/rebase state | Need git-dir files |
| Work-tree commands (status, checkout, stash) | Need index + work tree |
| Linked worktrees, rerere, sparse-checkout | Need their admin/state files |
| Hooks expecting another `git` to read refs/objects | Not supported for purely injected stores |
| `gc` / repack / on-disk commit-graph | Typed `UnsupportedObjectStore` unless prunable / in-memory graph (see above) |

The same matrix appears in [Object stores — No git directory at all](object-stores.md#no-git-directory-at-all). Injecting **only** the object store leaves refs and reflogs on disk as usual.

## Example: SQLite object database

`grit_examples::sqlite_odb::SqliteOdbStore` keeps zlib-compressed store bytes in SQLite (one row per object id). The runnable demo commits through a SQLite primary, walks history, and exports loose objects for system `git fsck`:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

```bash
cargo run -p grit-examples --example sqlite-object-store
```

Run the conformance-backed integration tests with:

```bash
cargo test -p grit-examples --features full-tests sqlite_repository_conformance
```

## Related pages

- [Object stores](object-stores.md) — `Odb` layering, alternates, pack ingest, benchmarks
- [Refs](refs.md) — `References` handle, reflog, branch tracking
- [Embedding](embedding.md) — `Environment`, diagnostics, subprocess injection

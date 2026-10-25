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

Built-in backends live under [`grit_lib::objects::odb::store`](rustdoc:grit_lib::objects::odb::store) (`LooseStore`, `FilesSource`, `MemoryStore`, `CompositeStore`, and others).

## Lookup order

For a given [`Odb`](rustdoc:grit_lib::objects::odb::Odb) handle:

1. In-memory overlay (when enabled for merge-tree-style operations)
2. **Primary** store — files source by default, or your custom [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore)
3. Extra read sources from `OdbBuilder::push_read_source`
4. Alternate object directories when `OdbBuilder::alternates` is true (`info/alternates`, environment alternates, submodule object dirs)

## Where the primary store lives

A files-backed primary is `<git_dir>/objects` (the common directory's `objects/` for a linked worktree). When the [`Environment`](rustdoc:grit_lib::config::environment::Environment) passed in [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) sets [`git_object_directory`](rustdoc:grit_lib::config::environment::Environment::git_object_directory) (`GIT_OBJECT_DIRECTORY`), that directory is the primary instead: reads and writes go there, and a relative path resolves against the environment's current directory. The directory must exist. As in Git, `<git_dir>/objects` is then reachable only as an alternate, through [`git_alternate_object_directories`](rustdoc:grit_lib::config::environment::Environment::git_alternate_object_directories) or the new directory's `info/alternates`. This is how hooks and quarantines keep new objects out of the shared store.

## Backend failures

An error from your store's `read`, `read_info` or `contains` (a dropped connection, a locked database) reaches the caller as that error, not as a missing object. To test for an object, call [`Odb::contains`](rustdoc:grit_lib::objects::odb::Odb::contains): it walks the lookup order like a read does and returns the first source failure, so it never reports an object as present that `Odb::read` cannot return. Fetch and push negotiation, write-tree, ref checks and rev-parse all use it, so an outage stops the operation with a retryable error instead of re-sending objects or reporting broken refs. [`Odb::exists`](rustdoc:grit_lib::objects::odb::Odb::exists) is the infallible form: it treats a failing source as not having the object, so keep it for hints where a wrong answer is harmless.

## Building a custom repository

Pass an [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) rooted at the repository's `objects/` directory (or at `GIT_OBJECT_DIRECTORY` when the environment sets it) to [`RepositoryBuilder::odb_builder`](rustdoc:grit_lib::repo::RepositoryBuilder::odb_builder). Any path naming that directory works: one through symlinks (such as macOS `/var` → `/private/var` or `/tmp` → `/private/tmp`) or one relative to the environment's current directory. The opened [`Odb`](rustdoc:grit_lib::objects::odb::Odb) reports the canonical path. A builder rooted anywhere else fails with [`Error::OdbObjectsDirMismatch`](rustdoc:grit_lib::error::Error::OdbObjectsDirMismatch).

<!-- include: grit-examples/src/bin/memory_primary.rs -->

```bash
cargo run -p grit-examples --bin memory_primary
```

## No git directory at all

When both the object store and the ref store are injected, [`RepositoryBuilder::open`](rustdoc:grit_lib::repo::RepositoryBuilder::open) reads nothing from the git directory's layout: it may be empty or missing. Repository config is still read from `<git_dir>/config` when that file exists, and the index lives at `<git_dir>/index`, so create the directory before staging files. Seed `HEAD` with [`initial_branch`](rustdoc:grit_lib::repo::RepositoryBuilder::initial_branch):

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::RepositoryBuilder;

let git_dir = std::path::Path::new("/nowhere/in-memory.git");
let repo = RepositoryBuilder::new(RepositoryOptions::empty())
    .odb_builder(
        OdbBuilder::files(git_dir.join("objects"))
            .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .alternates(false),
    )
    .ref_store(Arc::new(MemoryRefStore::new()))
    .initial_branch("main")
    .open(git_dir, None)?;
# Ok::<(), grit_lib::error::Error>(())
```

## What works with a custom primary

Object reads and writes go through your store, so commits, staging (`porcelain::add::stage` and auto-staging commits), log, diff, merge, stash, notes and bundle creation all work.

[`Repository::fsck`](rustdoc:grit_lib::repo::Repository::fsck) works too: it lists the store's objects with `ObjectStore::for_each_object`, re-hashes each one against its id and runs the usual object and connectivity checks. Read or hash failures are reported as `FsckIssue::BadObject`, since there is no loose file path to name.

Some operations still install or verify packs under the on-disk `objects/` directory. With a custom primary they fail up front with [`Error::UnsupportedObjectStore`](rustdoc:grit_lib::error::Error::UnsupportedObjectStore), before they touch refs, the remote or `objects/`. The `operation` field names the refused call:

| `operation` | Refused API |
| ----------- | ----------- |
| `fetch` | [`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch), `repo.remotes().fetch` / `fetch_all`, pull, promisor lazy fetch (every transport, bundles included) |
| `push` | [`Remote::push`](rustdoc:grit_lib::remote::Remote::push), `repo.remotes().push` |
| `gc`, `commit-graph write`, `repack`, `midx write`, `prune` | Filesystem maintenance (`Odb::gc`, `Odb::write_commit_graph`, `Odb::pack_store`, pack install, MIDX write) |

`repo.bundles().unbundle` reports the same refusal as [`BundleError::UnsupportedObjectStore`](rustdoc:grit_lib::bundle::BundleError::UnsupportedObjectStore) with operation `unbundle`.

To exchange objects with Git from a custom store today, export them as loose objects (as the examples below do) or fetch and push from a files-backed repository. Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Short ids and prefix lookup

Resolving a short id and abbreviating an id (for example [`abbreviate_object_id`](rustdoc:grit_lib::revwalk::rev_parse::abbreviate_object_id)) both call [`ObjectStore::lookup_prefix`](rustdoc:grit_lib::objects::odb::store::ObjectStore::lookup_prefix) once per id. A backend only has to implement `for_each_object`: the default `lookup_prefix` parses the prefix once into an [`ObjectIdPrefix`](rustdoc:grit_lib::objects::ObjectIdPrefix) and compares raw id bytes without allocating, so each call is one pass over the whole store (about 1 ms per 200 000 ids on the factory VM).

If your backend keeps ids sorted (a B-tree, an ordered key-value store, an SQL index on the raw id), override `lookup_prefix` with a range scan. Every id that starts with the prefix lies between [`ObjectIdPrefix::lowest`](rustdoc:grit_lib::objects::ObjectIdPrefix::lowest) and [`ObjectIdPrefix::highest`](rustdoc:grit_lib::objects::ObjectIdPrefix::highest), inclusive, and nothing else does:

```rust
use std::collections::BTreeSet;

use grit_lib::error::Result;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectIdPrefix};

fn lookup_prefix(
    ids: &BTreeSet<ObjectId>,
    prefix: &str,
    limit: usize,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    let prefix = ObjectIdPrefix::from_hex(prefix, HashAlgo::Sha1)?;
    let matches = ids.range(prefix.lowest()..=prefix.highest());
    out.extend(matches.take(if limit == 0 { usize::MAX } else { limit }));
    Ok(())
}
```

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites (and `grit_test_support::refstore_conformance` does the same for ref stores, see [Refs](refs.md#writing-a-ref-backend)). Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

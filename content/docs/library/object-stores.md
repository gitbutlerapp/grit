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

[`Odb::read_store`](rustdoc:grit_lib::objects::odb::Odb::read_store) returns that whole chain as one `Arc<dyn ObjectStore>`, whatever the primary is. Hand it to another repository's `push_read_source` to layer one repository under another (a read-through cache or a mirror), or pass it as the `bases` of `ingest_pack_into`. A missing object is `Ok(None)` and never starts a promisor fetch. Its `open_stream` buffers the payload; for a large blob call [`Odb::with_stream`](rustdoc:grit_lib::objects::odb::Odb::with_stream) on the `Odb` itself.

The narrower accessors keep their meaning: [`Odb::sources`](rustdoc:grit_lib::objects::odb::Odb::sources) is the alternates alone (step 4), and [`Odb::primary`](rustdoc:grit_lib::objects::odb::Odb::primary) returns the files-backed primary and fails with `UnsupportedObjectStore` on a custom one. Use [`Odb::primary_writable`](rustdoc:grit_lib::objects::odb::Odb::primary_writable) for the primary of any kind.

```rust
use std::sync::Arc;

use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};

let upstream = OdbBuilder::files("/nowhere/upstream/objects")
    .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
    .alternates(false)
    .build();
let mirror = OdbBuilder::files("/nowhere/mirror/objects")
    .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
    .push_read_source(upstream.read_store())
    .alternates(false)
    .build();
let oid = upstream.write(ObjectKind::Blob, b"shared")?;
assert!(mirror.contains(&oid)?);
# Ok::<(), grit_lib::error::Error>(())
```

`Arc<T>`, `Box<T>` and `&T` implement `ObjectStore` (and `WritableObjectStore`) whenever `T` does, `dyn` included, by forwarding every method. A decorator written as `struct Counted<S: ObjectStore>(S)` can therefore wrap a shared `Arc<dyn ObjectStore>` and still be passed on as `Arc<dyn ObjectStore>`, with no hand-written forwarding newtype.

## Where the primary store lives

A files-backed primary is `<git_dir>/objects` (the common directory's `objects/` for a linked worktree). When the [`Environment`](rustdoc:grit_lib::config::environment::Environment) passed in [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) sets [`git_object_directory`](rustdoc:grit_lib::config::environment::Environment::git_object_directory) (`GIT_OBJECT_DIRECTORY`), that directory is the primary instead: reads and writes go there, and a relative path resolves against the environment's current directory. The directory must exist, and an empty value fails to open, as in Git. Also as in Git, `<git_dir>/objects` is then reachable only as an alternate, through [`git_alternate_object_directories`](rustdoc:grit_lib::config::environment::Environment::git_alternate_object_directories) or the new directory's `info/alternates`. This is how hooks and quarantines keep new objects out of the shared store. Fetch, pull and push through [`Repository::remotes`](rustdoc:grit_lib::repo::Repository::remotes) read and write through the repository's own store, and the git-dir based fetch functions open the repository with `FetchOptions::environment`.

## Backend failures

An error from your store's `read`, `read_info` or `contains` (a dropped connection, a locked database) reaches the caller as that error, not as a missing object. To test for an object, call [`Odb::contains`](rustdoc:grit_lib::objects::odb::Odb::contains): it walks the lookup order like a read does and returns the first source failure, so it never reports an object as present that `Odb::read` cannot return. Fetch and push negotiation, write-tree, ref checks and rev-parse all use it, so an outage stops the operation with a retryable error instead of re-sending objects or reporting broken refs. [`Odb::exists`](rustdoc:grit_lib::objects::odb::Odb::exists) is the infallible form: it treats a failing source as not having the object, so keep it for hints where a wrong answer is harmless.

A damaged copy ([`Error::Zlib`](rustdoc:grit_lib::error::Error::Zlib) or [`Error::CorruptObject`](rustdoc:grit_lib::error::Error::CorruptObject)) inside a [`CompositeStore`](rustdoc:grit_lib::objects::odb::store::CompositeStore), including the alternates chain, is treated differently from an outage: the read moves on to the next child and returns its good copy if it has one. When no child can return the object, the caller gets the first corruption error rather than a missing object, so bit rot in one replica is never mistaken for an absent object.

## Building a custom repository

Pass an [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) rooted at the repository's `objects/` directory (or at `GIT_OBJECT_DIRECTORY` when the environment sets it) to [`RepositoryBuilder::odb_builder`](rustdoc:grit_lib::repo::RepositoryBuilder::odb_builder). Any path naming that directory works: one through symlinks (such as macOS `/var` → `/private/var` or `/tmp` → `/private/tmp`) or one relative to the environment's current directory. The opened [`Odb`](rustdoc:grit_lib::objects::odb::Odb) reports the canonical path. A builder rooted anywhere else fails with [`Error::OdbObjectsDirMismatch`](rustdoc:grit_lib::error::Error::OdbObjectsDirMismatch).

The custom primary and every read source must hash with the repository's object format: the `extensions.objectformat` value in `<git_dir>/config`, or SHA-1 when that key or the config file is missing. Create the store with the same [`HashAlgo`](rustdoc:grit_lib::objects::HashAlgo) the repository was initialized with (`MemoryStore::new(HashAlgo::Sha256)` for a SHA-256 repository). A store that reports another algorithm through `ObjectStore::hash_algo` fails to open with [`Error::ObjectFormatMismatch`](rustdoc:grit_lib::error::Error::ObjectFormatMismatch), whose `repository` and `store` fields name the two algorithms, instead of writing object ids the repository's refs, index and config disagree with. Calling `OdbBuilder::build` on its own does no check, since there is no repository config to compare against.

<!-- include: grit-examples/src/bin/memory_primary.rs -->

```bash
cargo run -p grit-examples --bin memory_primary
```

## No git directory at all

When both the object store and the ref store are injected, [`RepositoryBuilder::open`](rustdoc:grit_lib::repo::RepositoryBuilder::open) reads nothing from the git directory's layout: it may be empty or missing. Repository config is still read from `<git_dir>/config` when that file exists. The index lives at `<git_dir>/index`: staging creates the git directory when it is missing, and a commit then writes `COMMIT_EDITMSG` and the index there. Seed `HEAD` with [`initial_branch`](rustdoc:grit_lib::repo::RepositoryBuilder::initial_branch):

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

Hooks still run, but there is nothing on disk for another program to open. They start in the work tree (the git directory when there is none), and `GIT_DIR` is exported only when the [`Environment`](rustdoc:grit_lib::config::environment::Environment) you passed sets [`git_dir`](rustdoc:grit_lib::config::environment::Environment::git_dir); opening a path through `RepositoryBuilder::open` does not set it. Even when it is set, objects and refs held in memory are invisible to the system `git`, so a `pre-commit` hook that runs `git status` or `git rev-parse` fails with "not a git repository". Keep hooks for such repositories to work that only needs the work tree and the hook's arguments.

## What works with a custom primary

Object reads and writes go through your store, so commits, staging (`porcelain::add::stage` and auto-staging commits), log, diff, merge, stash, notes, bundle creation and unbundling all work.

[`Repository::fsck`](rustdoc:grit_lib::repo::Repository::fsck) works too: it lists the store's objects with `ObjectStore::for_each_object`, re-hashes each one against its id and runs the usual object and connectivity checks. Read or hash failures are reported as `FsckIssue::BadObject`, since there is no loose file path to name.

[`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch), [`Remote::push`](rustdoc:grit_lib::remote::Remote::push), pull, bundle fetch, and [`clone_into`](rustdoc:grit_lib::remote::clone::clone_into) read and write objects through the custom primary (pack bytes are resolved into the store with [`ingest_received_pack`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack)). Partial-clone filters are not supported yet (`operation: "partial fetch"`). Some operations still install or verify packs under the on-disk `objects/` directory:

| `operation` | Refused API |
| ----------- | ----------- |
| `partial fetch` | Fetch or clone with an object filter on a custom primary |
| `gc`, `commit-graph write`, `repack`, `midx write`, `prune` | Filesystem maintenance (`Odb::gc`, `Odb::write_commit_graph`, `Odb::pack_store`, pack install with `index_pack::install_pack_*`, MIDX write) |

Maintenance stays files-only by design: packs, `.idx`, multi-pack-index and commit-graph files are how the files backend stores objects, and a custom store owns its own layout. A custom store still gets a commit-graph, built in memory and kept in its own storage (see [Commit-graphs for a custom store](#commit-graphs-for-a-custom-store)). Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Fast history walks

Rev-list, merge-base and path-limited log read every commit they cross through [`ObjectStore::read_into`](rustdoc:grit_lib::objects::odb::store::ObjectStore::read_into), into one buffer the walk reuses. Implement it to copy or decode your stored bytes straight into that buffer. The default goes through `read`, which allocates an owned `Object` per commit before copying it. Without a commit-graph, a store that implements `read_into` walks history about as fast as the files backend: on a 120,000-commit history, counting every commit takes the same time through a pass-through store as through the files backend.

## Commit-graphs for a custom store

A commit-graph lets walks skip reading commit objects entirely. Parents, root trees, committer dates and generation numbers come from the graph, and so do changed-path Bloom filters for `log -- <path>`. On the 120,000-commit history above, counting commits drops from about 650 ms to about 40 ms.

Graph files under `objects/info/` describe the files layout, not your store, so grit never reads them for a custom primary: a stale or foreign graph there cannot make a walk report commits your store does not have. Instead:

1. Build the graph through any `Odb` with [`build_commit_graph`](rustdoc:grit_lib::maintenance::build_commit_graph) (or `Repository::build_commit_graph`). It walks every commit reachable from the repository's refs and returns a [`CommitGraphBuild`](rustdoc:grit_lib::maintenance::CommitGraphBuild): either a [`BuiltCommitGraph`](rustdoc:grit_lib::maintenance::BuiltCommitGraph) or, in a shallow repository or one with grafts or replace refs, the [`CommitGraphSkipReason`](rustdoc:grit_lib::maintenance::CommitGraphSkipReason) no graph may be built.
2. Store `BuiltCommitGraph::bytes` wherever your backend keeps its data. They are byte for byte the file Git writes to `objects/info/commit-graph`, and `git commit-graph verify` accepts them.
3. When you open the repository, parse the bytes with [`CommitGraphChain::from_bytes`](rustdoc:grit_lib::revwalk::commit_graph::CommitGraphChain::from_bytes) and attach them with [`OdbBuilder::commit_graph`](rustdoc:grit_lib::objects::odb::OdbBuilder::commit_graph).

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{CommitGraphBuild, CommitGraphOptions};
use grit_lib::objects::odb::store::WritableObjectStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::revwalk::commit_graph::CommitGraphChain;

fn open(
    git_dir: &std::path::Path,
    store: Arc<dyn WritableObjectStore>,
    graph: Option<Vec<u8>>,
) -> grit_lib::error::Result<Repository> {
    let mut odb = OdbBuilder::files(git_dir.join("objects"))
        .primary(store)
        .alternates(false);
    if let Some(bytes) = graph {
        odb = odb.commit_graph(Arc::new(CommitGraphChain::from_bytes(bytes)?));
    }
    RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(odb)
        .open(git_dir, None)
}

fn refresh_graph(repo: &Repository) -> grit_lib::error::Result<Option<Vec<u8>>> {
    match repo.build_commit_graph(&CommitGraphOptions::with_changed_paths(true))? {
        CommitGraphBuild::Built(graph) => Ok(Some(graph.bytes)),
        CommitGraphBuild::Skipped(_) => Ok(None),
    }
}
```

An attached graph is trusted, not checked against the store: every commit it lists must be in the store with the parents and tree it records, which holds for a graph built from the same repository. Commits written after the graph was built are not in it and are read from the store as usual, so rebuilding now and then (with your other maintenance) is enough. With changed paths on, a rebuild reuses the Bloom filters of the attached graph and only computes filters for new commits. The graph's hash algorithm must match the repository's object format, or opening fails with [`Error::ObjectFormatMismatch`](rustdoc:grit_lib::error::Error::ObjectFormatMismatch). `OdbBuilder::commit_graph` works for a files primary too, and takes the place of its on-disk graph.

### Benchmarks

A 120,000-commit linear history (350 files, 44 MB `.git`, one pack) on a 4-vCPU Linux VM, release build, best of three, in milliseconds. The custom store is a pass-through over the same repository's files backend, so only the cost of the store interface differs. Git 2.43 timings are hyperfine means of the matching commands.

| Operation | Git, no graph | Files backend | Custom store, before | Custom store | Git, graph | Custom store, attached graph |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `rev-list --count HEAD` | 425 | 651 | 1129 | 682 | 75 | 41 |
| `rev-list --count HEAD~100000..HEAD` | 356 | 285 | 661 | 304 | 67 | 110 |
| `merge-base HEAD~60000 HEAD~10` | 196 | 161 | 381 | 163 | 36 | 40 |
| `log -n 20 -- dir7/f3.txt` | 53 | 43 | 95 | 52 | 10 | 8 |

"Before" is grit-lib before custom-store reads stopped installing the files backend's pack-read context on every object. The graph columns use changed-path Bloom filters. Peak memory for all four walks in one process is about 187 MB for the files backend and for the custom store without a graph, and 46 MB with the attached graph.

## Receiving packs into a custom store

Fetch, clone, push and bundles move objects as a pack stream. [`ingest_pack_into`](rustdoc:grit_lib::objects::unpack_objects::ingest_pack_into) applies one to any [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore): it inflates each object, resolves `OFS_DELTA` and `REF_DELTA` chains, verifies the pack checksum, and calls `write` for every object. A *thin* pack (one whose deltas name bases the sender knew you have) resolves those bases from the `bases` store you pass, after looking in the target store itself. Objects up to 1 MiB stay in memory until the pack is resolved; larger blobs are read back from your store when a delta needs them.

The repository-level entry points use it automatically. [`ingest_received_pack`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack) and [`ingest_received_pack_path`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack_path) write into a custom primary instead of installing `objects/pack/pack-*.pack`, reading thin bases through the whole `Odb`; the returned [`IngestedPack`](rustdoc:grit_lib::objects::index_pack::IngestedPack) has `pack_path: None`. [`Bundle::unbundle`](rustdoc:grit_lib::bundle::Bundle::unbundle) streams the bundle's pack straight from the file into your store, with no temporary copy.

<!-- include: grit-examples/src/bin/ingest_pack_into_store.rs -->

```bash
cargo run -p grit-examples --bin ingest_pack_into_store
```

For a lower-level hook, [`fetch_remote_into`](rustdoc:grit_lib::remote::fetch::fetch_remote_into) drives wire negotiation on an open [`Connection`](rustdoc:grit_lib::remote::transport::Connection) and ingests into the repository's primary store. With [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions) configured to spool the negotiated pack without index-pack, you receive a temp pack file and ingest it yourself with [`ingest_received_pack_path`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack_path) or [`ingest_pack_into`](rustdoc:grit_lib::objects::unpack_objects::ingest_pack_into). Going the other way, [`build_pack`](rustdoc:grit_lib::objects::pack_objects::build_pack) on the custom repository's `Odb` produces a pack that `git index-pack --strict` accepts.

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

## Batched existence checks

When grit needs to know whether many objects exist at once, it calls [`ObjectStore::contains_many`](rustdoc:grit_lib::objects::odb::store::ObjectStore::contains_many) with the whole batch and expects one answer per id, in input order. Write-tree (and so every commit) uses it to confirm that the cached trees of unchanged directories still exist. Fetch and push negotiation use it to sort out which ids are already present. The default implementation calls `contains` once per id. A store that pays per call, such as a database or a network service, should override it to answer the batch in one query.

With an override, committing one changed file costs a handful of store calls however many directories the repository has: one batch for the reused trees, one for the changed directory's files, and an existence check before each of the objects the commit writes. Subtrees grit has just confirmed or written are not probed again. The reuse batch covers every layer of the database: the primary first, then each [`OdbBuilder::push_read_source`](rustdoc:grit_lib::objects::odb::OdbBuilder::push_read_source) store and the alternates, each asked only about the trees earlier layers lack. An empty local cache in front of a slow read source therefore costs one batch to that source per commit, not one call per directory.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites (and `grit_test_support::refstore_conformance` does the same for ref stores, see [Refs](refs.md#writing-a-ref-backend)). Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

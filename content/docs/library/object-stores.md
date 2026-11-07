---
title: Object stores
summary: Pluggable ObjectStore backends, RepositoryOptions::with_object_store, OdbBuilder layers, and the odb_conformance test suite.
---

By default [`Repository`](rustdoc:grit_lib::repo::Repository) opens a files-backed [`Odb`](rustdoc:grit_lib::objects::odb::Odb) (loose objects, packs, and optional MIDX). Embedders swap the **primary** store with [`RepositoryOptions::with_object_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_object_store), and add read-only layers, an attached commit-graph or an alternates override with an [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) passed to [`RepositoryOptions::with_odb_builder`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_odb_builder). Neither takes a path: [`Repository::open`](rustdoc:grit_lib::repo::Repository::open) resolves the `objects/` directory itself.

## Traits

| Trait | Role |
| ----- | ---- |
| [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) | Read, metadata, streaming, enumeration |
| [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore) | Insert objects (idempotent writes) |
| [`PrunableObjectStore`](rustdoc:grit_lib::objects::odb::store::PrunableObjectStore) | Optional: delete objects, so `gc` and `prune` work on the store (see [Garbage collection on a custom store](#garbage-collection-on-a-custom-store)) |

Built-in backends live under [`grit_lib::objects::odb::store`](rustdoc:grit_lib::objects::odb::store) (`LooseStore`, `FilesSource`, `MemoryStore`, `CompositeStore`, and others).

## Lookup order

For a given [`Odb`](rustdoc:grit_lib::objects::odb::Odb) handle:

1. In-memory overlay (when enabled for merge-tree-style operations)
2. **Primary** store — files source by default, or your custom [`WritableObjectStore`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore)
3. Extra read sources from `OdbBuilder::push_read_source`
4. Alternate object directories (`info/alternates`, environment alternates, submodule object dirs). They are on for the files primary and off for a custom primary unless `OdbBuilder::alternates(true)` turns them on; `OdbBuilder::alternates(false)` turns them off for the files primary.

[`Odb::read_store`](rustdoc:grit_lib::objects::odb::Odb::read_store) returns that whole chain as one `Arc<dyn ObjectStore>`, whatever the primary is. Hand it to another repository's `push_read_source` to layer one repository under another (a read-through cache or a mirror), or pass it as the `bases` of `ingest_pack_into`. A missing object is `Ok(None)` and never starts a promisor fetch. Its `open_stream` buffers the payload; for a large blob call [`Odb::with_stream`](rustdoc:grit_lib::objects::odb::Odb::with_stream) on the `Odb` itself.

The narrower accessors keep their meaning: [`Odb::sources`](rustdoc:grit_lib::objects::odb::Odb::sources) is the alternates alone (step 4), and [`Odb::primary`](rustdoc:grit_lib::objects::odb::Odb::primary) returns the files-backed primary and fails with `UnsupportedObjectStore` (`files_source`) on a custom one. Use [`Odb::primary_writable`](rustdoc:grit_lib::objects::odb::Odb::primary_writable) for the primary of any kind.

```rust
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::Repository;

let in_memory = || {
    RepositoryOptions::empty()
        .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
        .with_ref_store(Arc::new(MemoryRefStore::new()))
};
let upstream = Repository::open(Path::new("/nowhere/upstream.git"), &in_memory())?;
let mirror = Repository::open(
    Path::new("/nowhere/mirror.git"),
    &in_memory().with_odb_builder(OdbBuilder::new().push_read_source(upstream.odb().read_store())),
)?;
let oid = upstream.odb().write(ObjectKind::Blob, b"shared")?;
assert!(mirror.odb().contains(&oid)?);
# Ok::<(), grit_lib::error::Error>(())
```

`Arc<T>`, `Box<T>` and `&T` implement `ObjectStore` (and `WritableObjectStore` and `PrunableObjectStore`) whenever `T` does, `dyn` included, by forwarding every method, `as_prunable` among them. A decorator written as `struct Counted<S: ObjectStore>(S)` can therefore wrap a shared `Arc<dyn ObjectStore>` and still be passed on as `Arc<dyn ObjectStore>`, with no hand-written forwarding newtype.

## Where the primary store lives

A files-backed primary is `<git_dir>/objects` (the common directory's `objects/` for a linked worktree). When the [`Environment`](rustdoc:grit_lib::config::environment::Environment) passed in [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) sets [`git_object_directory`](rustdoc:grit_lib::config::environment::Environment::git_object_directory) (`GIT_OBJECT_DIRECTORY`), that directory is the primary instead: reads and writes go there, and a relative path resolves against the environment's current directory. The directory must exist, and an empty value fails to open, as in Git. Also as in Git, `<git_dir>/objects` is then reachable only as an alternate, through [`git_alternate_object_directories`](rustdoc:grit_lib::config::environment::Environment::git_alternate_object_directories) or the new directory's `info/alternates`. This is how hooks and quarantines keep new objects out of the shared store. Fetch, pull and push through [`Repository::remotes`](rustdoc:grit_lib::repo::Repository::remotes) read and write through the repository's own store, and the git-dir based fetch functions open the repository with `FetchOptions::environment`.

## Backend failures

An error from your store's `read`, `read_info` or `contains` (a dropped connection, a locked database) reaches the caller as that error, not as a missing object. To test for an object, call [`Odb::contains`](rustdoc:grit_lib::objects::odb::Odb::contains): it walks the lookup order like a read does and returns the first source failure, so it never reports an object as present that `Odb::read` cannot return. Fetch and push negotiation, write-tree, ref checks and rev-parse all use it, so an outage stops the operation with a retryable error instead of re-sending objects or reporting broken refs. [`Odb::exists`](rustdoc:grit_lib::objects::odb::Odb::exists) is the infallible form: it treats a failing source as not having the object, so keep it for hints where a wrong answer is harmless.

A damaged copy ([`Error::Zlib`](rustdoc:grit_lib::error::Error::Zlib) or [`Error::CorruptObject`](rustdoc:grit_lib::error::Error::CorruptObject)) inside a [`CompositeStore`](rustdoc:grit_lib::objects::odb::store::CompositeStore), including the alternates chain, is treated differently from an outage: the read moves on to the next child and returns its good copy if it has one. When no child can return the object, the caller gets the first corruption error rather than a missing object, so bit rot in one replica is never mistaken for an absent object.

## Building a custom repository

Pass your store to [`RepositoryOptions::with_object_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_object_store) when calling [`Repository::open`](rustdoc:grit_lib::repo::Repository::open), or when creating the repository with [`Repository::init`](rustdoc:grit_lib::repo::Repository::init) (see [Creating a repository on your stores](#creating-a-repository-on-your-stores)). That is the only way to replace the files primary. The repository still resolves its `objects/` directory (`GIT_OBJECT_DIRECTORY` included) and the opened [`Odb`](rustdoc:grit_lib::objects::odb::Odb) reports its canonical path, but no object is read from or written to it and it need not exist. With `GIT_OBJECT_DIRECTORY` set, the custom store still wins and the environment's directory is left untouched. Add read sources, a commit-graph or an alternates override with an [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder) through [`RepositoryOptions::with_odb_builder`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_odb_builder); it works with the files primary as well.

The custom primary and every read source must hash with the repository's object format: the `extensions.objectformat` value in `<git_dir>/config`, or SHA-1 when that key or the config file is missing. Create the store with the same [`HashAlgo`](rustdoc:grit_lib::objects::HashAlgo) the repository was initialized with (`MemoryStore::new(HashAlgo::Sha256)` for a SHA-256 repository). A store that reports another algorithm through `ObjectStore::hash_algo` fails to open with [`RepositoryError::ObjectFormatMismatch`](rustdoc:grit_lib::error::RepositoryError::ObjectFormatMismatch), whose `repository` and `store` fields name the two algorithms, instead of writing object ids the repository's refs, index and config disagree with.

## Creating a repository on your stores

[`Repository::init`](rustdoc:grit_lib::repo::Repository::init) takes the same [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) through [`InitOptions::with_repository_options`](rustdoc:grit_lib::repo::InitOptions::with_repository_options) and creates only what your stores do not own:

| Injected | What init writes in the git directory |
| -------- | ------------------------------------- |
| Object store only | Everything except `objects/` (no `objects/info/`, no `objects/pack/`). |
| Ref store only | Everything except `HEAD` and `refs/`. `HEAD` is seeded in your store at the initial branch. |
| Both | Only `config` (and, for a non-bare repository, the work tree beside it). |

The config holds `core.repositoryformatversion`, `core.bare` and, for a SHA-256 store, `extensions.objectformat`. You need not set [`InitOptions::object_format`](rustdoc:grit_lib::repo::InitOptions::object_format): init takes it from the store's `ObjectStore::hash_algo`. When you do set it, or when you reinitialize an existing repository, the formats must agree; otherwise init fails with [`RepositoryError::ObjectFormatMismatch`](rustdoc:grit_lib::error::RepositoryError::ObjectFormatMismatch) before anything is written. Asking for reftable together with an injected ref store fails with [`RepositoryError::InitRefStorageWithInjectedRefStore`](rustdoc:grit_lib::error::RepositoryError::InitRefStorageWithInjectedRefStore): the ref store you pass is the ref backend.

The returned [`Repository`](rustdoc:grit_lib::repo::Repository) is already wired to your stores; there is no need to open it again.

<!-- include: grit-examples/src/bin/memory_primary.rs -->

```bash
cargo run -p grit-examples --bin memory_primary
```

## No git directory at all

When both the object store and the ref store are injected, [`Repository::open`](rustdoc:grit_lib::repo::Repository::open) reads nothing from the git directory's layout: it may be empty or missing. Repository config is still read from `<git_dir>/config` when that file exists. The index lives at `<git_dir>/index`: staging creates the git directory when it is missing, and a commit then writes `COMMIT_EDITMSG` and the index there. Seed `HEAD` with [`RepositoryOptions::with_initial_branch`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_initial_branch), or create the repository with `Repository::init` as above, which seeds it for you:

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::refs::store::MemoryRefStore;

let git_dir = std::path::Path::new("/nowhere/in-memory.git");
let repo = Repository::open(
    git_dir,
    &RepositoryOptions::empty()
        .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
        .with_ref_store(Arc::new(MemoryRefStore::new()))
        .with_initial_branch("main"),
)?;
# Ok::<(), grit_lib::error::Error>(())
```

What still touches the git directory on disk with both stores injected: `<git_dir>/config` (read when present), the index and the files a commit leaves beside it (`COMMIT_EDITMSG`, operation state for merge, revert and rebase), hook scripts under `hooks/`, and linked-worktree admin directories. With only the object store injected, refs, reflogs and `HEAD` stay in the git directory as usual.

Hooks still run, but there is nothing on disk for another program to open. They start in the work tree (the git directory when there is none), and `GIT_DIR` is exported only when the [`Environment`](rustdoc:grit_lib::config::environment::Environment) you passed sets [`git_dir`](rustdoc:grit_lib::config::environment::Environment::git_dir); opening a path through [`Repository::open`](rustdoc:grit_lib::repo::Repository::open) does not set it. Even when it is set, objects and refs held in memory are invisible to the system `git`, so a `pre-commit` hook that runs `git status` or `git rev-parse` fails with "not a git repository". Keep hooks for such repositories to work that only needs the work tree and the hook's arguments.

## What works with a custom primary

Object reads and writes go through your store, so commits, staging (`porcelain::add::stage` and auto-staging commits), log, diff, merge (including cherry-pick and revert), stash, notes, bundle creation and unbundling all work.

[`Repository::fsck`](rustdoc:grit_lib::repo::Repository::fsck) works too: it lists the store's objects with `ObjectStore::for_each_object`, re-hashes each one against its id and runs the usual object and connectivity checks. Read or hash failures are reported as `FsckIssue::BadObject`, since there is no loose file path to name.

[`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch), [`Remote::push`](rustdoc:grit_lib::remote::Remote::push), pull, bundle fetch, [`clone_into`](rustdoc:grit_lib::remote::clone::clone_into), and smart HTTP serving ([`UploadPack`](rustdoc:grit_lib::remote::serve::UploadPack) / [`ReceivePack`](rustdoc:grit_lib::remote::serve::ReceivePack)) read and write objects through the custom primary (pack bytes are resolved into the store with [`ingest_received_pack`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack) or the receive-pack overlay described below). Partial-clone filters are not supported yet.

An operation that needs the on-disk `objects/` layout refuses a custom primary with [`RepositoryError::UnsupportedObjectStore`](rustdoc:grit_lib::repo::RepositoryError::UnsupportedObjectStore). Its `operation` field is a [`FilesOnlyOperation`](rustdoc:grit_lib::repo::error::FilesOnlyOperation), so match the variant rather than the message; `FilesOnlyOperation::as_str` gives the name below, which `grit --json` reports as `operation` with `kind` `unsupported_object_store` (see [Exit codes](../../exit-codes/)). The table is generated from the enum's rustdoc:

<!-- grit:variant-table grit_lib::repo::error::FilesOnlyOperation -->

Repack, multi-pack-index and commit-graph writes stay files-only by design: packs, `.idx`, multi-pack-index and commit-graph files are how the files backend stores objects, and a custom store owns its own layout. A custom store still gets a commit-graph, built in memory and kept in its own storage (see [Commit-graphs for a custom store](#commit-graphs-for-a-custom-store)), and garbage collection when it can delete objects (next section). Use `Odb::files_objects_dir` when you need the on-disk `objects/` path only for files-backed repos.

## Garbage collection on a custom store

A store that can delete objects implements [`PrunableObjectStore`](rustdoc:grit_lib::objects::odb::store::PrunableObjectStore) and returns `Some(self)` from [`WritableObjectStore::as_prunable`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore::as_prunable). `MemoryStore` does. Then these work on a repository whose primary is that store:

| API | What it does on a custom store |
| --- | --- |
| [`maintenance::prune_loose_objects`](rustdoc:grit_lib::maintenance::prune_loose_objects) | Deletes unreachable objects older than `gc.pruneExpire` (or `PruneOptions::older_than`) |
| [`maintenance::gc`](rustdoc:grit_lib::maintenance::gc) | Pack-refs, reflog expiry, then the same prune; repack, drop-large-blobs and the commit-graph write are skipped |
| [`Odb::gc`](rustdoc:grit_lib::objects::odb::Odb::gc) | Deletes every unreachable object, with no grace period |
| [`refs::gc::prune_loose_unreachable`](rustdoc:grit_lib::refs::gc::prune_loose_unreachable) | Deletes what your own root set does not reach |

Reachability is the same as for the files backend: every ref, every reflog entry (including linked worktrees' reflogs), `HEAD` and the index of each worktree, and the stash; walks stop at shallow boundaries. Grit lists your objects with `ObjectStore::for_each_object`, walks the graph once, passes the unreachable ids to `PrunableObjectStore::remove` in batches, and calls `WritableObjectStore::flush` at the end.

The grace period comes from [`PrunableObjectStore::object_mtime`](rustdoc:grit_lib::objects::odb::store::PrunableObjectStore::object_mtime), which plays the part of a loose file's modification time. It defaults to `None`, and an object without a known time is kept whenever a grace period applies: with the default `gc.pruneExpire` of `2.weeks.ago`, a store that records no times (like `MemoryStore`) loses nothing until you prune with `now`. Record a time on `write` and `freshen` to get Git's behavior. The clock a relative expiry is measured from is passed in (`PruneOptions::with_now`, `GcOptions::now_unix`).

With `extensions.preciousObjects` set, `prune_loose_objects` and `Odb::gc` fail with `RepositoryError::PreciousObjectsRepository` (on the files backend too), and `maintenance::gc` on a custom store skips its prune step (`GcStats::prune_ran` stays false).

A store whose `as_prunable` is `None` is never pruned: those calls fail with `UnsupportedObjectStore` and `operation` `prune` before touching anything, and never fall back to the `objects/` directory.

## Filesystem-only by design

Some parts of grit-lib read or write `objects/` on disk directly, because what they work on is the files layout itself or a directory that only exists on disk. None of them runs against your store's objects:

| Area | Why it needs `objects/` on disk |
| ---- | ------------------------------- |
| The files backend | The files primary inside `Odb`, the loose, pack and multi-pack-index stores, and the pack readers and writers they are built from. |
| Maintenance | `gc`, repack, prune, offload, commit-graph, multi-pack-index and pack bitmap writing rewrite the files layout. On a custom primary they are refused (see the table above). |
| Pack install | Installing a received pack as `objects/pack/pack-*.pack` (index-pack, bundle install, fetch's pack spool) and receive-pack's on-disk quarantine. A custom primary takes the ingest path instead. |
| Repository layout | Opening a repository with the default files primary, discovering a repository from a directory, and creating the directories of a new repository. |
| Submodules and alternates | A submodule's git dir and `objects/info/alternates` name directories on disk. Alternates are read for the files primary unless `OdbBuilder::alternates(false)`, and for a custom primary only with `OdbBuilder::alternates(true)`. |
| Local-path transport | Fetch, push and clone from a peer repository given as a local path open that repository's own files store. |
| Gated fast paths | Rev-list, merge-base, fsck and bitmap walks read packs directly only after checking that the primary is the files backend, or that a pack bitmap exists. A custom primary takes the general path through `ObjectStore`. |
| Git-directory fallbacks | Functions that take only a git directory path, such as `refs::write_ref`, `refs::pack_remote_tracking_refs_for_clone`, `remote::list_refs_from_git_dir` without an `Odb`, and `promisor_pack_object_ids`, have no repository to ask, so they use that directory's files database. Called through a `Repository`, the same work (ref updates, `packed-refs` peel lines, ls-refs peeling) goes through your store. |
| Scratch databases | Throwaway, empty databases in a temporary directory, such as the one the thin-pack check indexes a received pack against. They never hold your objects. |

A source-scan test in grit-lib (`storage_backend_guard`) keeps this list honest: any new direct access to `objects/` outside these areas fails the build, and the places that still bypass a custom store are tracked there until they go through it.

## Fast history walks

Rev-list, merge-base and path-limited log read every commit they cross through [`ObjectStore::read_into`](rustdoc:grit_lib::objects::odb::store::ObjectStore::read_into), into one buffer the walk reuses. Implement it to copy or decode your stored bytes straight into that buffer. The default goes through `read`, which allocates an owned `Object` per commit before copying it. Without a commit-graph, a store that implements `read_into` walks history about as fast as the files backend: on a 120,000-commit history, counting every commit takes the same time through a pass-through store as through the files backend.

## Commit-graphs for a custom store

A commit-graph lets walks skip reading commit objects entirely. Parents, root trees, committer dates and generation numbers come from the graph, and so do changed-path Bloom filters for `log -- <path>`. On the 120,000-commit history above, counting commits drops from about 650 ms to about 40 ms.

Graph files under `objects/info/` describe the files layout, not your store, so grit never reads them for a custom primary: a stale or foreign graph there cannot make a walk report commits your store does not have. Instead:

1. Build the graph through any `Odb` with [`build_commit_graph`](rustdoc:grit_lib::maintenance::build_commit_graph) (or `Repository::build_commit_graph`). It walks every commit reachable from the repository's refs and returns a [`CommitGraphBuild`](rustdoc:grit_lib::maintenance::CommitGraphBuild): either a [`BuiltCommitGraph`](rustdoc:grit_lib::maintenance::BuiltCommitGraph) or, in a shallow repository or one with grafts or replace refs, the [`CommitGraphSkipReason`](rustdoc:grit_lib::maintenance::CommitGraphSkipReason) no graph may be built.
2. Store `BuiltCommitGraph::bytes` wherever your backend keeps its data. They are byte for byte the file Git writes to `objects/info/commit-graph`, and `git commit-graph verify` accepts them.
3. When you open the repository, turn stored bytes into a chain with [`BuiltCommitGraph::to_chain`](rustdoc:grit_lib::maintenance::BuiltCommitGraph::to_chain) (or build fresh from step 1 and call `to_chain` on the result) and attach it with [`OdbBuilder::commit_graph`](rustdoc:grit_lib::objects::odb::OdbBuilder::commit_graph).

```rust
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{CommitGraphBuild, CommitGraphOptions};
use grit_lib::objects::odb::store::WritableObjectStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::repo::{Repository};
use grit_lib::maintenance::BuiltCommitGraph;

fn open(
    git_dir: &std::path::Path,
    store: Arc<dyn WritableObjectStore>,
    graph: Option<Vec<u8>>,
) -> grit_lib::error::Result<Repository> {
    let mut layers = OdbBuilder::new();
    if let Some(bytes) = graph {
        let built = BuiltCommitGraph { bytes };
        layers = layers.commit_graph(Arc::new(built.to_chain()?));
    }
    Repository::open(
        git_dir,
        &RepositoryOptions::empty()
            .with_object_store(store)
            .with_odb_builder(layers),
    )
}

fn refresh_graph(repo: &Repository) -> grit_lib::error::Result<Option<Vec<u8>>> {
    match repo.build_commit_graph(&CommitGraphOptions::with_changed_paths(true))? {
        CommitGraphBuild::Built(graph) => Ok(Some(graph.bytes)),
        CommitGraphBuild::Skipped(_) => Ok(None),
    }
}
```

An attached graph is trusted, not checked against the store: every commit it lists must be in the store with the parents and tree it records, which holds for a graph built from the same repository. Commits written after the graph was built are not in it and are read from the store as usual, so rebuilding now and then (with your other maintenance) is enough. With changed paths on, a rebuild reuses the Bloom filters of the attached graph and only computes filters for new commits. The graph's hash algorithm must match the repository's object format, or opening fails with [`RepositoryError::ObjectFormatMismatch`](rustdoc:grit_lib::error::RepositoryError::ObjectFormatMismatch). `OdbBuilder::commit_graph` works for a files primary too, and takes the place of its on-disk graph.

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

The repository-level entry points use it automatically. [`ingest_received_pack`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack) and [`ingest_received_pack_path`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack_path) write into a custom primary instead of installing `objects/pack/pack-*.pack`, reading thin bases through the whole `Odb`; the returned [`IngestedPack`](rustdoc:grit_lib::objects::index_pack::IngestedPack) has `pack_path: None`. Each successful ingest ends with one [`Odb::flush`](rustdoc:grit_lib::objects::odb::Odb::flush) on the primary (one store round trip per new object during unpack, not a `contains` probe on read sources). [`Bundle::unbundle`](rustdoc:grit_lib::bundle::Bundle::unbundle) streams the bundle's pack straight from the file into your store, with no temporary copy.

[`ReceivePack`](rustdoc:grit_lib::remote::serve::ReceivePack) uses the same quarantine contract on a custom primary: the pack is spooled to a system temp file, unpacked object-by-object into an in-memory overlay whose read side is the repository's [`Odb::read_store`](rustdoc:grit_lib::objects::odb::Odb::read_store), and nothing is copied into your primary until pre-receive succeeds. Then the overlay objects are written through [`Odb::primary_writable`](rustdoc:grit_lib::objects::odb::Odb::primary_writable) and [`Odb::flush_for_publish`](rustdoc:grit_lib::objects::odb::Odb::flush_for_publish). A declined push or a corrupt pack drops the overlay with no primary writes. Shell hooks do not get `GIT_OBJECT_DIRECTORY` / `GIT_QUARANTINE_PATH` for a memory overlay (only the files quarantine sets them). Objects that were published and then lose a later ref update stay in the store, as on the files backend after the quarantine migrates, until garbage collection removes them: that needs a store with the prune capability (see [Garbage collection on a custom store](#garbage-collection-on-a-custom-store)).

<!-- include: grit-examples/src/bin/ingest_pack_into_store.rs -->

```bash
cargo run -p grit-examples --bin ingest_pack_into_store
```

For a lower-level hook, [`RepositoryRemotes::fetch_from`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::fetch_from) with [`TransferEndpoint::Connection`](rustdoc:grit_lib::remote::transfer::TransferEndpoint::Connection) drives wire negotiation on an open [`Connection`](rustdoc:grit_lib::remote::transport::Connection) and ingests into the repository's primary store. With [`FetchOptions`](rustdoc:grit_lib::remote::transfer::FetchOptions) configured to spool the negotiated pack without index-pack, you receive a temp pack file and ingest it yourself with [`ingest_received_pack_path`](rustdoc:grit_lib::objects::index_pack::ingest_received_pack_path) or [`ingest_pack_into`](rustdoc:grit_lib::objects::unpack_objects::ingest_pack_into). Going the other way, [`build_pack`](rustdoc:grit_lib::objects::pack_objects::build_pack) on the custom repository's `Odb` produces a pack that `git index-pack --strict` accepts.

## Short ids and prefix lookup

Resolving a short id and abbreviating an id (for example [`RevWalk::abbreviate`](rustdoc:grit_lib::revwalk::RevWalk::abbreviate)) both call [`ObjectStore::lookup_prefix`](rustdoc:grit_lib::objects::odb::store::ObjectStore::lookup_prefix) once per id. A backend only has to implement `for_each_object`: the default `lookup_prefix` parses the prefix once into an [`ObjectIdPrefix`](rustdoc:grit_lib::objects::ObjectIdPrefix) and compares raw id bytes without allocating, so each call is one pass over the whole store (about 1 ms per 200 000 ids on the factory VM).

For a label that does not need to resolve back to the id (log lines, conflict markers, status rows), [`ObjectId::to_short_hex`](rustdoc:grit_lib::objects::ObjectId::to_short_hex) returns the first seven hex digits without touching the store, and [`short_hex`](rustdoc:grit_lib::objects::short_hex) does the same for a hex string you already hold.

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

When grit needs to know whether many objects exist at once, it calls [`ObjectStore::contains_many`](rustdoc:grit_lib::objects::odb::store::ObjectStore::contains_many) with the whole batch and expects one answer per id, in input order. Write-tree (and so every commit) uses it to confirm that the cached trees of unchanged directories still exist. Fetch and push negotiation use it to sort out which ids are already present. Local-path and `file://` clone or fetch into an empty custom primary does not probe existence at all (every reachable object is written once). An incremental local fetch or push into a non-empty custom primary asks `contains_many` once over the candidate send set. The default implementation calls `contains` once per id. A store that pays per call, such as a database or a network service, should override it to answer the batch in one query.

With an override, committing one changed file costs a handful of store calls however many directories the repository has: one batch for the reused trees, one for the changed directory's files, and one [`WritableObjectStore::write`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore::write) per new object (no Odb-level existence probe on a custom primary). Subtrees grit has just confirmed or written are not probed again. The reuse batch covers every layer of the database: the primary first, then each [`OdbBuilder::push_read_source`](rustdoc:grit_lib::objects::odb::OdbBuilder::push_read_source) store and the alternates, each asked only about the trees earlier layers lack. An empty local cache in front of a slow read source therefore costs one batch to that source per commit, not one call per directory.

[`WritableObjectStore::flush`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore::flush) and [`WritableObjectStore::rollback`](rustdoc:grit_lib::objects::odb::store::WritableObjectStore::rollback) let backends batch many writes (one transaction, one fsync). Pack ingest and [`Odb::flush`](rustdoc:grit_lib::objects::odb::Odb::flush) before ref updates call flush on a custom primary; failed ingest calls rollback. Reads must observe successful writes before flush; flush is the durability barrier.

## Conformance tests

The workspace crate `grit_test_support::odb_conformance` provides shared read/write suites (and `grit_test_support::refstore_conformance` does the same for ref stores, see [Refs](refs.md#writing-a-ref-backend)). Run them from an integration test in your crate (see [`grit-lib/tests/odb_conformance_memory.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/odb_conformance_memory.rs)) to validate a custom backend before wiring it through [`OdbBuilder`](rustdoc:grit_lib::objects::odb::OdbBuilder).

## Example: append-only packfile KV store

`grit-examples` ships a single-file `PackfileKvStore` in `grit_examples::packfile_kv` (zlib records plus an in-memory index rebuilt on open) and a walkthrough that commits through a custom primary, walks history, and exports loose objects for system Git:

<!-- include: grit-examples/examples/custom-object-store.rs -->

## Example: SQLite object database

For embedders who want indexed lookup without maintaining a separate `objects/` shard tree, `grit_examples::sqlite_odb` provides [`SqliteOdbStore`](https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/sqlite_odb.rs): each object is a row keyed by raw object id with zlib-compressed canonical store bytes (the same on-disk payload as a loose object file). The demo commits through a SQLite primary, walks history, then exports loose objects so system `git fsck` and `git log` succeed:

<!-- include: grit-examples/examples/sqlite-object-store.rs -->

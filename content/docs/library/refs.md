---
title: Refs
summary: Repository ref handles, transactions, backend selection, and path-based helpers.
---

References name commits and other objects. Prefer an open [`Repository`](rustdoc:grit_lib::repo::Repository): call `refs()` to borrow the cached [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) selected for that repository, or `with_ref_store` to inject a custom backend (for example [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore) or the `custom_ref_store` example below).

Path-based helpers in [`refs`](rustdoc:grit_lib::refs) (`resolve_ref`, `list_refs`, `write_ref`, …) still work on a git directory; they open the same backend via [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store). Rev-parse and porcelain on a `Repository` always go through the handle's store.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and [`RepoCaches`](rustdoc:grit_lib::repo_caches::RepoCaches) construct [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

## Transactions

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend.

## Resolving HEAD and symbolic refs

[`resolve_head`](rustdoc:grit_lib::state::resolve_head) reads `HEAD` and returns a [`HeadState`](rustdoc:grit_lib::state::HeadState). [`resolve_ref`](rustdoc:grit_lib::refs::resolve_ref) (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. [`read_ref_file`](rustdoc:grit_lib::refs::read_ref_file) returns a [`Ref`](rustdoc:grit_lib::refs::Ref) without resolving the full chain.

## Listing, writing, and reflog

[`list_refs`](rustdoc:grit_lib::refs::list_refs) and [`list_refs_for_repository`](rustdoc:grit_lib::refs::list_refs_for_repository) iterate with a prefix. [`write_ref`](rustdoc:grit_lib::refs::write_ref) and [`write_symbolic_ref`](rustdoc:grit_lib::refs::write_symbolic_ref) update single refs; use transactions when updating many refs atomically.

Reflog helpers live in [`reflog`](rustdoc:grit_lib::reflog) and on [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (`for_each_reflog_entry`, `replace_reflog`, …). [`append_reflog`](rustdoc:grit_lib::refs::append_reflog) records an update; [`read_reflog`](rustdoc:grit_lib::reflog::read_reflog) reads entries back.

## Examples

**Library guide** — resolve HEAD, list branches and tags, update a demo branch (included on this page):

<!-- include: grit-examples/src/bin/guide_refs.rs -->

**Custom ref store** — wrap [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore) with operation counting and inject it via `Repository::with_ref_store`:

<!-- include: grit-examples/src/bin/custom_ref_store.rs -->

```bash
cargo run --bin guide_refs /path/to/repo
cargo run --bin custom_ref_store
```

---
title: Refs
summary: Repository ref handles, transactions, backend selection, and path-based helpers.
---

References name commits and other objects. Prefer `Repository::references` (`References`) for resolve/list/update on the repository’s ref backend. Call `Repository::refs` to borrow the cached `RefStore` trait object, or `with_ref_store` to inject a custom backend (for example `MemoryRefStore` or the `custom_ref_store` example below).

Path-based helpers in [`refs`](rustdoc:grit_lib::refs) (`resolve_ref`, `list_refs`, `write_ref`, …) still work on a git directory; they open the same backend via `open_ref_store`. Rev-parse and porcelain on a `Repository` always go through the handle's store.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::refs::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::refs::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and [`RepoCaches`](rustdoc:grit_lib::repo::caches::RepoCaches) construct [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

## Transactions

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::refs::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend.

## Resolving HEAD and symbolic refs

`resolve_head` reads `HEAD` and returns a `HeadState`. `resolve_ref` (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. `read_ref_file` returns a `Ref` without resolving the full chain.

## Listing, writing, and reflog

`list_refs` and `list_refs_for_repository` iterate with a prefix. `write_ref` and `write_symbolic_ref` update single refs; use transactions when updating many refs atomically.

Reflog helpers live in [`reflog`](rustdoc:grit_lib::refs::reflog) and on [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (`for_each_reflog_entry`, `replace_reflog`, …). [`append_reflog`](rustdoc:grit_lib::refs::append_reflog) records an update; [`read_reflog`](rustdoc:grit_lib::refs::reflog::read_reflog) reads entries back.

## Upstream, target branch, and ahead/behind

[`branch_tracking`](rustdoc:grit_lib::refs::branch_tracking) answers "how does this branch compare with its base?", the question `grit status` and `grit shortlog` show at the top. [`upstream_branch`](rustdoc:grit_lib::refs::branch_tracking::upstream_branch) resolves a branch's configured upstream, [`target_branch`](rustdoc:grit_lib::refs::branch_tracking::target_branch) finds the repository's trunk (the `target.branch` config value, then [`DEFAULT_TARGET_CANDIDATES`](rustdoc:grit_lib::refs::branch_tracking::DEFAULT_TARGET_CANDIDATES)), and [`comparison_base`](rustdoc:grit_lib::refs::branch_tracking::comparison_base) picks the upstream when it exists and the target otherwise (legacy helper). [`BranchStanding::compute`](rustdoc:grit_lib::refs::branch_tracking::BranchStanding::compute) returns separate [`TrackedComparison`](rustdoc:grit_lib::refs::branch_tracking::TrackedComparison) values for target and upstream for status. Each base resolves to a [`ComparisonBase`](rustdoc:grit_lib::refs::branch_tracking::ComparisonBase) with the display name, full ref, commit, and [`BaseSource`](rustdoc:grit_lib::refs::branch_tracking::BaseSource), through the repository's ref store.

[`TipComparison`](rustdoc:grit_lib::refs::branch_tracking::TipComparison)`::compute` then counts commits on each side (the same numbers as `git rev-list --left-right --count tip...base`), keeps the newest ahead commits up to a limit, and classifies the result as a [`Standing`](rustdoc:grit_lib::refs::branch_tracking::Standing): even, ahead, behind, or diverged.

```rust
use grit_lib::refs::branch_tracking::{comparison_base, Standing, TipComparison};

if let Some(base) = comparison_base(&repo, "main")? {
    let cmp = TipComparison::compute(&repo, head_oid, base.oid, 10)?;
    if cmp.standing() == Standing::Behind {
        println!("{} new commits on {}", cmp.behind, base.name);
    }
}
```

## Examples

**Library guide** — resolve HEAD, list branches and tags, update a demo branch (included on this page):

<!-- include: grit-examples/src/bin/guide_refs.rs -->

**Custom ref store** — wrap `MemoryRefStore` with operation counting and inject it via `Repository::with_ref_store`:

<!-- include: grit-examples/src/bin/custom_ref_store.rs -->

```bash
cargo run --bin guide_refs /path/to/repo
cargo run --bin custom_ref_store
```

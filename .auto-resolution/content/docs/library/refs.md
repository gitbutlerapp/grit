---
title: Refs
summary: Repository ref handles, transactions, backend selection, and path-based helpers.
---

References name commits and other objects. Prefer `Repository::references` (`References`) for resolve/list/update on the repository’s ref backend. Call `Repository::refs` to borrow the cached `RefStore` trait object, or `with_ref_store` to inject a custom backend (for example `MemoryRefStore` or the `custom_ref_store` example below).

Path-based helpers in [`refs`](rustdoc:grit_lib::refs) (`resolve_ref`, `list_refs`, `write_ref`, …) still work on a git directory; they open the same backend via `open_ref_store`. Rev-parse and porcelain on a `Repository` always go through the handle's store.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::refs::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::refs::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and [`RepoCaches`](rustdoc:grit_lib::repo::caches::RepoCaches) construct [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

## Lock files and contention

Loose ref updates, `packed-refs`, the index, config, commit-graph, and reftable `tables.list` all use Git-style lock files created exclusively (`O_CREAT|O_EXCL`). [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) centralizes acquisition, optional jittered exponential backoff, staged writes, and atomic rename. Backoff budgets come from repository config: `core.filesRefLockTimeout` (milliseconds, default 100; `0` fail-fast, `-1` retry indefinitely), `core.packedRefsTimeout` (milliseconds, default 1000 ms), and `reftable.lockTimeout` (milliseconds, default 1000 ms). Dropping a [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) without [`commit`](rustdoc:grit_lib::lockfile::LockFile::commit) removes only the lock file this handle created.

## Transactions

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::refs::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend.

## Resolving HEAD and symbolic refs

`resolve_head` reads `HEAD` and returns a `HeadState`. `resolve_ref` (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. `read_ref_file` returns a `Ref` without resolving the full chain.

On every backend, a missing ref fails with [`RefStoreError::NotFound`](rustdoc:grit_lib::refs::store::RefStoreError::NotFound) inside `Error::RefStore`, naming the ref that is absent. For a dangling symbolic ref (an unborn branch behind `HEAD`, say) that is the target, not the name you asked for. `delete_ref` on a missing ref fails the same way. Match on the variant rather than the message:

```rust
use grit_lib::error::Error;
use grit_lib::refs::store::RefStoreError;

match repo.refs().resolve("refs/heads/topic") {
    Ok(oid) => println!("topic is at {oid}"),
    Err(Error::RefStore(RefStoreError::NotFound { name })) => println!("{name} does not exist"),
    Err(err) => return Err(err.into()),
}
```

## Listing, writing, and reflog

`list_refs` and `list_refs_for_repository` iterate with a prefix. `write_ref` and `write_symbolic_ref` update single refs; use transactions when updating many refs atomically.

Reflog helpers live in [`reflog`](rustdoc:grit_lib::refs::reflog) and on [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (`for_each_reflog_entry`, `replace_reflog`, …). [`append_reflog`](rustdoc:grit_lib::refs::append_reflog) records an update; [`read_reflog`](rustdoc:grit_lib::refs::reflog::read_reflog) reads entries back.

## Branches and tags

[`porcelain::branch`](rustdoc:grit_lib::porcelain::branch) and [`porcelain::tag`](rustdoc:grit_lib::porcelain::tag) are what `grit branch` and `grit tag` call. [`list_branches`](rustdoc:grit_lib::porcelain::branch::list_branches) returns every local branch sorted by name, with the one `HEAD` is attached to marked as current. [`list_tags`](rustdoc:grit_lib::porcelain::tag::list_tags) returns each tag with its kind (lightweight or annotated) and the commit it peels to.

[`delete_branch`](rustdoc:grit_lib::porcelain::branch::delete_branch) refuses the current branch and branches checked out in another worktree. Unless `force` is set, it also requires the branch tip to be reachable from the branch's upstream, or from `HEAD` when there is no upstream (the same rule as `git branch -d`). Each refusal is a typed error: `BranchNotFound`, `CannotDeleteCurrentBranch`, `BranchCheckedOutInWorktree` or `BranchNotMerged`. [`delete_tag`](rustdoc:grit_lib::porcelain::tag::delete_tag) returns the object the tag pointed at, or `TagNotFound`. Both deletes run on the repository's ref store (so an injected store is honoured), compare-and-swap against the commit they checked, and remove the ref's reflog.

```rust
use grit_lib::error::Error;
use grit_lib::porcelain::branch::delete_branch;

match delete_branch(&repo, "topic", false) {
    Ok(deleted) => println!("deleted topic (was {})", deleted.oid),
    Err(Error::BranchNotMerged { .. }) => println!("topic has unmerged commits"),
    Err(err) => return Err(err),
}
```

## Upstream, target branch, and ahead/behind

[`branch_tracking`](rustdoc:grit_lib::refs::branch_tracking) answers "how does this branch compare with its base?", the question `grit status` and `grit shortlog` show at the top. [`upstream_branch`](rustdoc:grit_lib::refs::branch_tracking::upstream_branch) resolves a branch's configured upstream, [`target_branch`](rustdoc:grit_lib::refs::branch_tracking::target_branch) finds the repository's trunk (the `target.branch` config value, then [`DEFAULT_TARGET_CANDIDATES`](rustdoc:grit_lib::refs::branch_tracking::DEFAULT_TARGET_CANDIDATES)), and [`comparison_base`](rustdoc:grit_lib::refs::branch_tracking::comparison_base) picks the upstream when it exists and the target otherwise (legacy helper). [`BranchStanding::compute`](rustdoc:grit_lib::refs::branch_tracking::BranchStanding::compute) returns separate [`TrackedComparison`](rustdoc:grit_lib::refs::branch_tracking::TrackedComparison) values for target and upstream for status, with [`StatusHeader`](rustdoc:grit_lib::refs::branch_tracking::StatusHeader) for the human header line and pull hints. Each base resolves to a [`ComparisonBase`](rustdoc:grit_lib::refs::branch_tracking::ComparisonBase) with the display name, full ref, commit, and [`BaseSource`](rustdoc:grit_lib::refs::branch_tracking::BaseSource), through the repository's ref store.

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

---
title: Refs
summary: Repository ref handles, transactions, backend selection, and path-based helpers.
---

References name commits and other objects. Prefer `Repository::references` (`References`) for resolve/list/update on the repository’s ref backend. Call `Repository::refs` to borrow the cached `RefStore` trait object, or `with_ref_store` to inject a custom backend (for example `MemoryRefStore` or the `custom_ref_store` example below).

Path-based helpers in [`refs`](rustdoc:grit_lib::refs) (`resolve_ref`, `list_refs`, `write_ref`, …) still work on a git directory; they open the on-disk backend via `open_ref_store`, so they never see an injected store. Rev-parse and porcelain on a `Repository` always go through the handle's store.

## Injected stores and porcelain

Every porcelain operation that moves a ref writes through `Repository::refs`: commit, switch, branch, tag, merge ([`integrate_branch`](rustdoc:grit_lib::porcelain::merge::integrate_branch)), reset, stash, pick, revert and rebase. Each one reads `HEAD` with [`resolve_head_for_repository`](rustdoc:grit_lib::state::resolve_head_for_repository), updates refs and `HEAD` in the store, and appends reflog entries there. With an injected store such as `MemoryRefStore`, nothing is written to `.git/HEAD`, `.git/refs`, `.git/logs` or `packed-refs`. The index, objects and operation-state files (`MERGE_HEAD`, `ORIG_HEAD`, `rebase-merge/`, sequencer state) stay in the git directory.

To advance a checked-out branch from your own code, use [`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store). It checks that `HEAD` points at the branch, compares the tip with the expected old commit, then moves the branch and appends the branch and `HEAD` reflog entries allowed by `core.logAllRefUpdates`. On the files backend, the branch lock is held while the reflogs are appended. On every other backend, the branch update and its reflog entries commit as one transaction.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::refs::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::refs::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and [`RepoCaches`](rustdoc:grit_lib::repo::caches::RepoCaches) construct [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

## Lock files and contention

Loose ref updates, `packed-refs`, the index, config, commit-graph, and reftable `tables.list` all use Git-style lock files created exclusively (`O_CREAT|O_EXCL`). [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) centralizes acquisition, optional jittered exponential backoff, staged writes, and atomic rename. Backoff budgets come from repository config: `core.filesRefLockTimeout` (milliseconds, default 100; `0` fail-fast, `-1` retry indefinitely), `core.packedRefsTimeout` (milliseconds, default 1000 ms), and `reftable.lockTimeout` (milliseconds, default 1000 ms). Dropping a [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) without [`commit`](rustdoc:grit_lib::lockfile::LockFile::commit) removes only the lock file this handle created.

When `.git/index.lock` or a loose ref lock is still held after the budget, operations such as [`stage`](rustdoc:grit_lib::porcelain::add::stage), [`IndexLock::acquire`](rustdoc:grit_lib::index::IndexLock::acquire) and [`write_ref`](rustdoc:grit_lib::refs::write_ref) return [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld) with the lock `path` and nothing changed. With `core.lockfilePid` enabled, the owner writes its pid next to the lock (`index~pid.lock`), and `holder` carries it as a [`LockHolder`](rustdoc:grit_lib::lockfile::LockHolder): `running: false` means the lock is stale and safe to delete. Any [`LockError`](rustdoc:grit_lib::lockfile::LockError) converts into the same variant with `?`.

```rust
use grit_lib::error::Error;
use grit_lib::lockfile::LockHolder;

fn explain(err: &Error) -> Option<String> {
    let Error::LockHeld { path, holder } = err else {
        return None;
    };
    Some(match holder {
        Some(LockHolder { running: false, pid }) => {
            format!("{} is stale (process {pid} exited); remove it", path.display())
        }
        Some(LockHolder { pid, .. }) => format!("process {pid} holds {}", path.display()),
        None => format!("another process holds {}", path.display()),
    })
}
```

Reftable failures are typed as [`ReftableError`](rustdoc:grit_lib::refs::reftable::ReftableError) inside `Error::Reftable`: `Locked` when another writer holds `tables.list.lock`, `ExpectedMismatch` when a compare-and-swap update finds a different value, and corruption variants such as `BadMagic`, `FooterChecksumMismatch` or `Truncated` (which names the [`ReftableField`](rustdoc:grit_lib::refs::reftable::ReftableField) that was cut short). Through a [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore) transaction, a held lock surfaces as `RefStoreError::LockHeld` and a compare-and-swap failure as `RefStoreError::ExpectedMismatch`, the same variants the files backend returns.

## Transactions

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::refs::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend.

Every backend (files, reftable, memory, or your own wrapper around one) refuses the same malformed batches in `prepare`, before taking any lock. A name that is not safe to store (path traversal, a `.lock` suffix, spaces, empty or doubled components, a non-`refs/` name that is not an all-caps pseudo-ref like `ORIG_HEAD`), on an update or on either side of a reflog or case-only rename, fails with [`RefStoreError::InvalidRefName`](rustdoc:grit_lib::refs::store::RefStoreError::InvalidRefName). Storing the all-zero object id as a direct value fails with [`RefStoreError::NullOid`](rustdoc:grit_lib::refs::store::RefStoreError::NullOid), because Git reads such a ref as broken. To delete a ref, set `new_value: None`. Reflog-only updates may still record the null id.

## Resolving HEAD and symbolic refs

`resolve_head` reads `HEAD` from a git directory and returns a `HeadState`; `resolve_head_for_repository` reads it through the repository's store (an empty injected store gives `HeadState::Invalid`). `resolve_ref` (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. `read_ref_file` returns a `Ref` without resolving the full chain.

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

[`porcelain::branch`](rustdoc:grit_lib::porcelain::branch) and [`porcelain::tag`](rustdoc:grit_lib::porcelain::tag) are what `grit branch` and `grit tag` call. [`list_branches`](rustdoc:grit_lib::porcelain::branch::list_branches) returns every local branch sorted by name, with the one `HEAD` is attached to marked as current. [`list_tags`](rustdoc:grit_lib::porcelain::tag::list_tags) takes a [`RefQuery`](rustdoc:grit_lib::refs::listing::RefQuery) (patterns, sort, limit, `--contains`, `--points-at`) and returns complete [`TagSummary`](rustdoc:grit_lib::porcelain::tag::TagSummary) rows. Call [`list_tags_with_detail`](rustdoc:grit_lib::porcelain::tag::list_tags_with_detail) with [`TagDetail::Names`](rustdoc:grit_lib::porcelain::tag::TagDetail::Names) for names without tag-object reads, [`TagDetail::Kind`](rustdoc:grit_lib::porcelain::tag::TagDetail::Kind) for packed-peel classification, or [`TagDetail::Annotation`](rustdoc:grit_lib::porcelain::tag::TagDetail::Annotation) for annotation messages and taggers.

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

[`TipComparison`](rustdoc:grit_lib::refs::branch_tracking::TipComparison)`::compute` then counts commits on each side (the same numbers as `git rev-list --left-right --count tip...base`), keeps the newest ahead commits up to a limit, and classifies the result as a [`Standing`](rustdoc:grit_lib::refs::branch_tracking::Standing): even, ahead, behind, or diverged. For `grit shortlog`, [`compare_with_target_optional`](rustdoc:grit_lib::refs::branch_tracking::compare_with_target_optional) with [`TargetComparisonOptions`](rustdoc:grit_lib::refs::branch_tracking::TargetComparisonOptions) resolves the current branch and target (including `--target`) and returns [`TargetComparison`](rustdoc:grit_lib::refs::branch_tracking::TargetComparison) with bounded ahead and optional behind OID lists. For symmetric tip comparisons when you already have two commit ids, use [`TargetTipListOptions`](rustdoc:grit_lib::refs::branch_tracking::TargetTipListOptions) and [`TargetTipLists`](rustdoc:grit_lib::refs::branch_tracking::TargetTipLists)`::compute`.

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

Library code calls back into the store from iteration callbacks (for example, it reads each listed ref's reflog inside `for_each_reflog_ref`), so a custom backend must not hold an internal lock while it runs a `for_each_ref`, `for_each_reflog_entry` or `for_each_reflog_ref` callback. Collect what you need, or clone an `Arc` of the current state, and release the lock first. See [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) for the full contract.

```bash
cargo run --bin guide_refs /path/to/repo
cargo run --bin custom_ref_store
```

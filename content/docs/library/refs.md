---
title: Refs
summary: Repository ref handles, transactions, backend selection, and path-based helpers.
---

References name commits and other objects. Prefer `Repository::references` (`References`) for resolve/list/update on the repository’s ref backend. Call `Repository::refs` to borrow the cached `RefStore` trait object, or `with_ref_store` to inject a custom backend (for example `MemoryRefStore` or the `custom_ref_store` example below).

Path-based helpers in [`refs`](rustdoc:grit_lib::refs) (`resolve_ref`, `list_refs`, `write_ref`, …) still work on a git directory; they open the on-disk backend via `open_ref_store`, so they never see an injected store. Rev-parse and porcelain on a `Repository` always go through the handle's store.

## Injected stores and porcelain

Every porcelain operation that moves a ref writes through `Repository::refs`: commit, switch, branch, tag, merge ([`integrate_branch`](rustdoc:grit_lib::porcelain::merge::integrate_branch)), reset, stash, pick, revert and rebase. So does the history engine: planning and [`apply_rewrite`](rustdoc:grit_lib::revwalk::history::apply_rewrite) (reword, drop, squash, move, fixup, absorb), [`start_rebase`](rustdoc:grit_lib::revwalk::history::start_rebase), `rebase --exec` and [`undo_last_rewrite`](rustdoc:grit_lib::revwalk::history::undo_last_rewrite). Each one reads `HEAD` with [`resolve_head_for_repository`](rustdoc:grit_lib::state::resolve_head_for_repository), updates refs and `HEAD` in the store, and appends reflog entries there. With an injected store such as `MemoryRefStore`, nothing is written to `.git/HEAD`, `.git/refs`, `.git/logs` or `packed-refs`. The reflog handle, [`repo.reflogs()`](rustdoc:grit_lib::repo_handles::RepositoryReflogs), lists, reads, deletes and expires entries in that same store, and `gc` expires reflogs there too. The index, objects and operation-state files (`MERGE_HEAD`, `ORIG_HEAD`, `rebase-merge/`, sequencer state, the `grit/history-last` undo record) stay in the git directory.

The network operations keep the local side in the store the same way. [`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch) (local, `file://`, bundle and wire remotes) reads the previous tracking-ref values from it and writes the new tips, `--prune` deletions and `refs/remotes/<name>/HEAD` there. [`Remote::push`](rustdoc:grit_lib::remote::Remote::push) records the pushed tips as tracking refs there. [`RepositoryRemotes::prune`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::prune), [`set_head`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::set_head) and [`inspect`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::inspect) read and write the store too. Received objects and `FETCH_HEAD` still go to the git directory, and the destination of a local push keeps its own on-disk refs. Packing the imported refs into `packed-refs` after a clone only happens on a files repository.

Reads go through the store too. [`RevWalk`](rustdoc:grit_lib::revwalk::RevWalk) ref selections (`All`, `Branches`, `Tags`, `Remotes`, with or without a glob such as `v*`), `--stdin` pseudo-options ([`collect_revision_specs_from_stdin_lines`](rustdoc:grit_lib::revwalk::rev_list::collect_revision_specs_from_stdin_lines)), [`tag_targets`](rustdoc:grit_lib::revwalk::rev_list::tag_targets), `:/text` searches, `@{-N}` and the other rev-parse forms that read `HEAD` all list and resolve refs through `Repository::refs`. Linked worktree add/list/remove do as well (see [Worktree](../worktree/)).

To advance a checked-out branch from your own code, use [`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store). It checks that `HEAD` points at the branch, compares the tip with the expected old commit, then moves the branch and appends the branch and `HEAD` reflog entries allowed by `core.logAllRefUpdates`. On the files backend, the branch lock is held while the reflogs are appended. On every other backend, the branch update and its reflog entries commit as one transaction.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::refs::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::refs::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and [`RepoCaches`](rustdoc:grit_lib::repo::caches::RepoCaches) construct [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

### Reftable HEAD and linked worktrees

Reftable stores both symbolic and detached `HEAD` values in the worktree's own stack.
The common stack holds shared branches. The loose `HEAD` file serves repository
discovery only and does not override a stack record. Fresh reftable repositories
and linked worktrees initialize this layout before checkout, so system Git sees
the same branch and commit as `Repository::refs()`. Compaction also uses stack
refs and reflogs, so deleting HEAD cannot revive a stale loose value.

HEAD changes use the ref store's transaction, compare-and-swap and reflog handling.
Updating through an attached linked-worktree HEAD advances the shared branch and
writes its branch reflog in the common stack, with the HEAD reflog in the worktree
stack. These two stack commits have the partial-failure semantics documented on
[`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore).

### Reading `packed-refs`

The files backend maps `packed-refs` and keeps one view per store, shared by lookups, listings and peel snapshots. Opening it checks what Git checks: the file ends in a newline and its last line can hold an object id. When the header carries the `sorted` trait (Git and grit always write it), lookups and prefix listings binary-search the mapped bytes in place. They read only the lines they probe and parse strictly the lines a listing covers, so a command on a repository with tens of thousands of packed refs touches a few pages of the file. As in Git, the trait is trusted rather than verified on every read. The trust model:

- **Point lookups** (resolving one ref) trust the header completely. In a file that claims `sorted` but is not, a lookup can miss a ref, exactly as `git rev-parse` does.
- **Prefix listings** (`refs/heads/`, `refs/tags/`, …) validate every line in the range and the records just before and after it, including their object ids and peel lines. They check name order across those records too. If any are out of order, the listing reads the whole file instead of returning part of the range. A record misplaced somewhere the listing does not read goes unnoticed, as in Git.
- **Full scans** (all refs, `refs/` as a whole, `PackedRefs::load`) read every line. They reject a carriage return or invalid UTF-8 anywhere in the file, and they return every record whatever its order.

[`refs_fsck`](rustdoc:grit_lib::refs::refs_fsck::refs_fsck) (and `grit fsck`) reports a file whose records break the trait as `packedRefUnsorted`. A file without the trait is searched linearly for lookups and scanned in full for listings.

## Lock files and contention

Loose ref updates, `packed-refs`, the index, config, commit-graph, and reftable `tables.list` all use Git-style lock files created exclusively (`O_CREAT|O_EXCL`). [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) centralizes acquisition, optional jittered exponential backoff, staged writes, and atomic rename. Backoff budgets come from repository config: `core.filesRefLockTimeout` (milliseconds, default 100; `0` fail-fast, `-1` retry indefinitely), `core.packedRefsTimeout` (milliseconds, default 1000 ms), and `reftable.lockTimeout` (milliseconds, default 1000 ms). Dropping a [`LockFile`](rustdoc:grit_lib::lockfile::LockFile) without [`commit`](rustdoc:grit_lib::lockfile::LockFile::commit) removes only the lock file this handle created.

When `.git/index.lock` is still held after the budget, [`stage`](rustdoc:grit_lib::porcelain::add::stage) and [`IndexLock::acquire`](rustdoc:grit_lib::index::IndexLock::acquire) return [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld) with the lock `path`. The index is unchanged. Low-level loose-ref helpers use the same variant, while ref-store transactions retain `RefStoreError::LockHeld { name }`. Grit-owned index locks write a pid sidecar next to the lock (`index~pid.lock`) by default ([`grit_lockfile_pid_enabled`](rustdoc:grit_lib::lockfile::grit_lockfile_pid_enabled); set `core.lockfilePid=false` to opt out). The sidecar records the holder as a [`LockHolder`](rustdoc:grit_lib::lockfile::LockHolder); its `running` field records whether that process existed when checked. Converting [`LockError::Held`](rustdoc:grit_lib::lockfile::LockError::Held) with `?` preserves these fields; other `LockError` variants retain their I/O source. [`LockHolder::remove_if_stale`](rustdoc:grit_lib::lockfile::LockHolder::remove_if_stale) removes the lock and sidecar when the holder has exited (used during interrupted rewrite recovery; ordinary acquire never steals a foreign lock).

For callers migrating from `Error::IndexLocked`, match `Error::LockHeld { path, holder }` instead of `{ lock_path, holder }`, and import `LockHolder` from `grit_lib::lockfile` instead of `grit_lib::error`. [`LockHolder::read`](rustdoc:grit_lib::lockfile::LockHolder::read) replaces the removed prose helper `format_index_lock_blocked_detail` when inspecting a pid sidecar directly. [`pid_path_for_lock`](rustdoc:grit_lib::lockfile::pid_path_for_lock) names the sidecar path for a lock file.

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

Ref-store transactions follow the same budget. [`RefStore::prepare`](rustdoc:grit_lib::refs::store::RefStore::prepare) fails fast with `RefStoreError::LockHeld { name }` while another writer holds a lock it needs, and `name` is always one of the failed transaction's own refs. On the files backend that is the per-ref `<ref>.lock` (or `packed-refs.lock` when the batch rewrites packed refs), so updates to disjoint refs run concurrently and only the same ref contends. A store with one writer slot for everything (such as [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore), or a database with a single write transaction) reports the same variant for any overlapping writer, even one updating a different ref; grit-lib's ref writes retry that through [`prepare_with_lock_wait`](rustdoc:grit_lib::refs::prepare_with_lock_wait) with jittered backoff for `core.filesRefLockTimeout` (100 ms by default, `0` to fail fast, `-1` to wait indefinitely), so disjoint refs on those backends still all land while a transaction held open past the budget fails. [`RefStore::ref_lock_wait`](rustdoc:grit_lib::refs::store::RefStore::ref_lock_wait) selects that budget: the files backend reads its git directory, a bare [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore) uses Git's default, and the store from [`Repository::refs`](rustdoc:grit_lib::repo::Repository::refs) (including an injected backend) reads `core.filesRefLockTimeout` from the open repository's git directory so porcelain and library commit helpers honor the same setting as on disk. Time spent inside `prepare` counts against the budget, so backends that already wait for their own lock files are not waited for twice. Call `prepare_with_lock_wait` yourself when you prepare transactions directly:

```rust
use grit_lib::lockfile::LockWait;
use grit_lib::refs::prepare_with_lock_wait;
use grit_lib::refs::store::{RefStore, RefTransaction};

fn apply(store: &dyn RefStore, txn: RefTransaction) -> grit_lib::error::Result<()> {
    let wait = LockWait::files_ref_lock(None);
    prepare_with_lock_wait(store, txn, wait)?.commit()?;
    Ok(())
}
```

Reftable failures are typed as [`ReftableError`](rustdoc:grit_lib::refs::reftable::ReftableError) inside `Error::Reftable`: `Locked` when another writer holds `tables.list.lock`, `ExpectedMismatch` when a compare-and-swap update finds a different value, and corruption variants such as `BadMagic`, `FooterChecksumMismatch` or `Truncated` (which names the [`ReftableField`](rustdoc:grit_lib::refs::reftable::ReftableField) that was cut short). Through a [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore) transaction, a held lock surfaces as `RefStoreError::LockHeld` and a compare-and-swap failure as `RefStoreError::ExpectedMismatch`, the same variants the files backend returns.

## Transactions

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::refs::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend.

To fill a new repository, such as the refs a clone imports, mark the batch with [`RefTransaction::initial`](rustdoc:grit_lib::refs::store::RefTransaction::initial), the equivalent of Git's initial ref transaction. When every update creates a direct ref that does not exist yet (no reflog, `Expected::Any` or `Expected::Missing`), the files backend checks names against one listing of the store and writes the whole batch as a single sorted `packed-refs` rewrite, with peel lines for annotated tags, under `packed-refs.lock` only. It writes no loose file or lock per ref, so 200,000 refs cost one file write. Any other batch, and every other backend, commits as usual. Only use it where no other writer creates the same refs at the same time.

Every backend (files, reftable, memory, or your own wrapper around one) refuses the same malformed batches in `prepare`, before taking any lock. A name that is not safe to store (path traversal, a `.lock` suffix, spaces, empty or doubled components, a non-`refs/` name that is not an all-caps pseudo-ref like `ORIG_HEAD`), on an update or on either side of a reflog or case-only rename, fails with [`RefStoreError::InvalidRefName`](rustdoc:grit_lib::refs::store::RefStoreError::InvalidRefName). A symbolic value whose target is not safe to store, would make `HEAD` point outside `refs/`, or points a ref at itself, fails with [`RefStoreError::InvalidSymrefTarget`](rustdoc:grit_lib::refs::store::RefStoreError::InvalidSymrefTarget). The target ref does not have to exist yet (unborn branches and aliases to not-yet-created refs stay valid). Storing the all-zero object id as a direct value fails with [`RefStoreError::NullOid`](rustdoc:grit_lib::refs::store::RefStoreError::NullOid), because Git reads such a ref as broken. To delete a ref, set `new_value: None`. Reflog-only updates may still record the null id.

The store a repository hands out, [`Repository::refs`](rustdoc:grit_lib::repo::Repository::refs), also checks every transaction against the repository's objects before any lock, as `git update-ref` does. This covers injected stores, [`References::update`](rustdoc:grit_lib::refs::References::update) and the branch move of [`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store) (its files-backend fast path runs the same check before it takes the branch lock), and the path-based writers ([`write_ref`](rustdoc:grit_lib::refs::write_ref), [`commit_ref_store_batch`](rustdoc:grit_lib::refs::commit_ref_store_batch)) make the same check against the git directory's objects. A direct value naming an object the repository does not store fails with [`RefStoreError::MissingObject`](rustdoc:grit_lib::refs::store::RefStoreError::MissingObject). A branch (`refs/heads/…`) or `HEAD` pointed at anything but a commit fails with [`RefStoreError::NotACommit`](rustdoc:grit_lib::refs::store::RefStoreError::NotACommit). Tags, remote-tracking refs and other refs may name any object. The lookup reads object headers only and never lazily fetches from a promisor remote. The empty tree counts only once it is actually stored, because `git fsck` reports a ref to an unstored one as missing. An unreadable object fails with [`RefStoreError::ObjectLookup`](rustdoc:grit_lib::refs::store::RefStoreError::ObjectLookup). Deletes and symbolic values are not looked up; reflog-only updates and explicit reflog object ids are checked the same way. One bad update fails the whole batch, and every ref stays unchanged.

Because of this check, writing a commit and then moving a branch to it with a compare-and-swap update is safe against a concurrent `git gc --prune=now`. If the prune removes the commit before the update, the update fails with `MissingObject` and does not leave a branch pointing at a missing commit. To write a dangling ref on purpose (for example, a corrupt-repository fixture), set [`RefUpdateFlags::skip_oid_verification`](rustdoc:grit_lib::refs::store::RefUpdateFlags::skip_oid_verification) on the update. A backend opened on its own, such as [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) or a bare `MemoryRefStore`, does not look at objects.

## Reset validation

[`reset_to_commit`](rustdoc:grit_lib::porcelain::reset::reset_to_commit) validates
[`ResetOptions::identity`](rustdoc:grit_lib::porcelain::reset::ResetOptions::identity)
before changing any repository state. If a caller modifies a public
[`Signature`](rustdoc:grit_lib::ident::Signature) field to an invalid value, reset
returns [`Error::InvalidSignature`](rustdoc:grit_lib::error::Error::InvalidSignature)
with HEAD, branch refs, index bytes, worktree files, reflogs, and operation state
unchanged. This includes merge autostashes and `refs/stash`.

The same validation applies to
[`preview_reset`](rustdoc:grit_lib::porcelain::reset::preview_reset) and dry runs,
even when the target is already HEAD or `core.logAllRefUpdates=false`. This is a
preflight guarantee for invalid signatures; later I/O errors can leave partial
updates.

## Resolving HEAD and symbolic refs

`resolve_head` reads `HEAD` from a git directory and returns a `HeadState`; `resolve_head_for_repository` reads it through the repository's store (an empty injected store gives `HeadState::Invalid`). When you only need the commit `HEAD` is on, call [`Repository::head_commit`](rustdoc:grit_lib::repo::Repository::head_commit): it reads through the same store and fails with `Error::UnbornHead` on a branch with no commits yet, or `Error::InvalidHead` when `HEAD` names nothing. `resolve_ref` (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. `read_ref_file` returns a `Ref` without resolving the full chain.

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

Parse a newest-first `ref@{n}` entry with [`ReflogSelector`](rustdoc:grit_lib::refs::reflog::ReflogSelector)
and pass its `ref_name` and `index` to `repo.reflogs().entries` or `delete`.
[`parse_reflog_ref`](rustdoc:grit_lib::refs::reflog::parse_reflog_ref) validates a reference alone:
`HEAD` stays `HEAD`, `main` becomes `refs/heads/main`, and already valid multi-component
names are retained as supplied. Neither parser reads the repository or checks existence.
Malformed selectors return `Error::ReflogInvalidSelector`, invalid or overflowing indices
return `Error::ReflogInvalidIndex` (with the integer error as its source), and invalid
reference names return `Error::ReflogUnknownRef`. The handle checks for missing logs
and out-of-range indices when you query or delete entries. It works on
`Repository::refs` (`for_each_reflog_ref`, `for_each_reflog_entry`, `replace_reflog`),
so it behaves the same on the files, reftable and injected backends; on the files
backend, newest-first queries read the log backwards and stop at the limit.

[`list_refs`](rustdoc:grit_lib::refs::list_refs), [`write_ref`](rustdoc:grit_lib::refs::write_ref), [`write_symbolic_ref`](rustdoc:grit_lib::refs::write_symbolic_ref), [`resolve_ref`](rustdoc:grit_lib::refs::resolve_ref), [`delete_ref`](rustdoc:grit_lib::refs::delete_ref), [`read_head`](rustdoc:grit_lib::refs::read_head), [`read_symbolic_ref`](rustdoc:grit_lib::refs::read_symbolic_ref), [`list_refs_glob`](rustdoc:grit_lib::refs::list_refs_glob), [`resolve_ref_dwim`](rustdoc:grit_lib::refs::resolve_ref_dwim), [`update_branch_for_commit`](rustdoc:grit_lib::refs::update_branch_for_commit), and [`list_refs_physical`](rustdoc:grit_lib::refs::list_refs_physical) are **on-disk only**: each opens the files or reftable backend via [`open_ref_store`](rustdoc:grit_lib::refs::store::open_ref_store) and bypasses [`Repository::refs`](rustdoc:grit_lib::repo::Repository::refs) and any injected store. With a [`Repository`](rustdoc:grit_lib::repo::Repository) handle, use [`References`](rustdoc:grit_lib::refs::References) (`resolve`, `list`, `update`), [`list_refs_for_repository`](rustdoc:grit_lib::refs::list_refs_for_repository), [`read_symbolic_ref_for_repository`](rustdoc:grit_lib::refs::read_symbolic_ref_for_repository), [`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store), and [`commit_ref_update_on_store`](rustdoc:grit_lib::refs::commit_ref_update_on_store) instead.

`list_refs_for_repository` and `References::list` iterate with a prefix on the repository's ref backend. Use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction) when updating many refs atomically.

Reflog helpers live in [`reflog`](rustdoc:grit_lib::refs::reflog) and on [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (`for_each_reflog_entry`, `replace_reflog`, …). [`append_reflog`](rustdoc:grit_lib::refs::append_reflog) records an update; [`read_reflog`](rustdoc:grit_lib::refs::reflog::read_reflog) reads entries back.

## Branches and tags

[`porcelain::branch`](rustdoc:grit_lib::porcelain::branch) and [`porcelain::tag`](rustdoc:grit_lib::porcelain::tag) are what `grit branch` and `grit tag` call. [`list_branches`](rustdoc:grit_lib::porcelain::branch::list_branches) returns every local branch sorted by name, with the one `HEAD` is attached to marked as current. [`list_tags`](rustdoc:grit_lib::porcelain::tag::list_tags) takes a [`RefQuery`](rustdoc:grit_lib::refs::listing::RefQuery) (patterns, sort, limit, `--contains`, `--points-at`) and returns complete [`TagSummary`](rustdoc:grit_lib::porcelain::tag::TagSummary) rows. Call [`list_tags_with_detail`](rustdoc:grit_lib::porcelain::tag::list_tags_with_detail) with [`TagDetail::Names`](rustdoc:grit_lib::porcelain::tag::TagDetail::Names) for names without tag-object reads, [`TagDetail::Kind`](rustdoc:grit_lib::porcelain::tag::TagDetail::Kind) for packed-peel classification, or [`TagDetail::Annotation`](rustdoc:grit_lib::porcelain::tag::TagDetail::Annotation) for annotation messages and taggers.

[`delete_branch`](rustdoc:grit_lib::porcelain::branch::delete_branch) refuses the current branch and branches checked out in another worktree. With [`DeleteMode::Safe`](rustdoc:grit_lib::porcelain::branch_error::DeleteMode::Safe) it also requires the branch tip to be reachable from the branch's upstream, or from `HEAD` when there is no upstream (the same rule as `git branch -d`).

Each refusal is an `Error::Branch(BranchError::...)`: `NotFound` for a missing branch, `CurrentlyCheckedOut { name }` for the branch named by this repository's own `HEAD`, `CheckedOut { name, worktree }` for a branch held by another worktree, or `NotFullyMerged` for an unmerged tip.

The current-branch check reads `HEAD` through the repository's store, including injected stores, even when the on-disk `HEAD` differs. `DeleteMode::Force` skips only the merge-state check; both it and dry runs retain the checked-out safety checks and leave HEAD and refs unchanged on refusal. The CLI maps both checked-out variants to the JSON kind `branch_checked_out`.

[`delete_tag`](rustdoc:grit_lib::porcelain::tag::delete_tag) returns the object the tag pointed at, or `TagNotFound`. Both deletes run on the repository's ref store (so an injected store is honoured), compare-and-swap against the commit they checked, and remove the ref's reflog.

```rust
use grit_lib::error::Error;
use grit_lib::porcelain::branch::{delete_branch, BranchError, DeleteMode};

let dry_run = false;
match delete_branch(&repo, "topic", DeleteMode::Safe, dry_run) {
    Ok(deleted) => println!("deleted topic (was {})", deleted.oid),
    Err(Error::Branch(BranchError::CurrentlyCheckedOut { name })) => {
        println!("{name} is the current branch");
    }
    Err(Error::Branch(BranchError::CheckedOut { name, worktree })) => {
        println!("{name} is held by {worktree}");
    }
    Err(Error::Branch(BranchError::NotFullyMerged { .. })) => {
        println!("topic has unmerged commits");
    }
    Err(err) => return Err(err),
}
```

[`query_refs`](rustdoc:grit_lib::refs::listing::query_refs) shares ancestry results between evaluated tips for finite, name-ordered `contains` queries. It stops after `RefQuery::limit` matching rows, preserving patterns, namespace unions, reverse ordering, and annotated-tag peeling. Filter targets are validated even for a zero limit or an empty candidate set. Unlimited queries, version/date sorts, and combined reachability or object filters retain the shared batch walk.

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

## Writing a ref backend

A custom [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (a database, a network service, an in-process map) does not have to re-derive Git's ref-update rules. [`refs::store::backend`](rustdoc:grit_lib::refs::store::backend) has the helpers the built-in stores use:

| Helper | What it does |
| ------ | ------------ |
| [`check_transaction`](rustdoc:grit_lib::refs::store::backend::check_transaction) | Every check `prepare` must make: ref names, directory/file conflicts (`a` vs `a/b`), compare-and-swap through symbolic refs, two updates writing the same ref. Cost is `O` of the transaction's names, symref depth, and directory probes—even when `RefBatchState` holds the whole store |
| [`apply_transaction`](rustdoc:grit_lib::refs::store::backend::apply_transaction) | The refs and reflogs a commit stores, including the reflog line on both a symbolic ref and its target |
| [`RefBatchState`](rustdoc:grit_lib::refs::store::backend::RefBatchState) | The ref and reflog maps both helpers work on |
| [`resolve_symbolic`](rustdoc:grit_lib::refs::store::backend::resolve_symbolic) | Follow symbolic refs inside a `RefBatchState` (for `RefEntry::peeled`) |
| [`validate_storable_refname`](rustdoc:grit_lib::refs::store::backend::validate_storable_refname), [`verify_create_conflicts`](rustdoc:grit_lib::refs::store::backend::verify_create_conflicts), [`expected_matches`](rustdoc:grit_lib::refs::store::backend::expected_matches) | The individual checks, for stores that only load part of their refs |

The usual shape: in `prepare`, call [`validate_transaction`](rustdoc:grit_lib::refs::store::backend::validate_transaction) (names and null ids, before any lock), take your lock (when another writer holds it, return `RefStoreError::LockHeld` naming the transaction's first ref right away, without spinning or queueing; grit-lib's callers retry for `core.filesRefLockTimeout`), load the refs the transaction touches into a `RefBatchState`, call `check_transaction`, and return a prepared value that owns the lock. In `commit`, run `apply_transaction` in place on that map when you own it (clone only when iteration callbacks still share the state) and persist the difference. The prepared value must release its lock in `Drop` too, because callers bail out with `?` between prepare and commit. Iteration callbacks may call back into the store, so release internal locks before running them (iterate over a snapshot). In `for_each_ref`, treat a non-empty prefix as a literal name prefix (`"HEAD"` lists `HEAD`), but list only refs under `refs/` for the empty prefix, leaving out root refs such as `HEAD` and `ORIG_HEAD`, as `git for-each-ref` does. `MemoryRefStore` is written exactly this way. Report your own storage failures as [`RefStoreError::Backend`](rustdoc:grit_lib::refs::store::RefStoreError::Backend) (`RefStoreError::backend(err)` keeps the error downcastable) and return [`RefStorageFormat::Custom`](rustdoc:grit_lib::refs::store::RefStorageFormat::Custom) from `format()`. Your backend never sees an object database. Once you install it with [`RepositoryBuilder::ref_store`](rustdoc:grit_lib::repo::RepositoryBuilder::ref_store), the repository runs the missing-object and branch-kind checks described under [Transactions](#transactions) before it calls your `prepare`. Leave `RefStore::verify_update_targets` at its default; the repository's store supplies that check.

Check the store with the conformance suite every built-in backend runs, `grit_test_support::refstore_conformance` (see [`grit-lib/tests/refstore_custom_backend.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/refstore_custom_backend.rs) for a complete map-backed store):

```rust
use std::sync::Arc;

use grit_lib::refs::store::RefStore;
use grit_test_support::refstore_conformance::run_suite;

#[test]
fn my_store_conforms() {
    run_suite(|| Arc::new(MyStore::default()) as Arc<dyn RefStore>);
}
```

To open a repository on your store, pass it to [`RepositoryBuilder::ref_store`](rustdoc:grit_lib::repo::RepositoryBuilder::ref_store). The git directory then needs no `HEAD` or `refs/`, and [`RepositoryBuilder::initial_branch`](rustdoc:grit_lib::repo::RepositoryBuilder::initial_branch) points `HEAD` at an unborn branch when the store has none. See [Object stores](object-stores.md) for opening with no git directory at all.

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

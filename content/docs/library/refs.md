---
title: Refs
summary: Repository ref handles, HEAD, reflogs, operation state, transactions and backend selection.
---

References name commits and other objects. Everything you do with them goes through [`repo.references()`](rustdoc:grit_lib::refs::References): `head`, `resolve`, `symbolic`, `dwim`, `list`, `list_glob`, `update`, `update_direct`, `update_symbolic`, `compare_and_swap`, `delete`, `compare_and_delete`, and `apply` / `apply_each` for batches. Reflogs are on [`repo.reflogs()`](rustdoc:grit_lib::repo_handles::RepositoryReflogs), and paused merges, rebases, picks and reverts on [`repo.operation()`](rustdoc:grit_lib::operation::Operations). Call `Repository::ref_store` to borrow the cached `RefStore` trait object, or `with_ref_store` to inject a custom backend (for example `MemoryRefStore` or the `custom_ref_store` example below). All of these work on the repository's store, so an injected store is always honoured.

```rust
use grit_lib::state::HeadState;

let refs = repo.references();
if let HeadState::Branch { refname, oid: Some(tip), .. } = refs.head()? {
    refs.update("refs/heads/backup", &tip)?;
    println!("{refname} is at {tip}");
}
for (name, oid) in refs.list_glob("refs/tags/v*")? {
    println!("{name} {oid}");
}
if let Some(found) = refs.dwim("main")? {
    println!("main is {} ({} candidates)", found.name, found.candidates);
}
```

## Injected stores and porcelain

Every porcelain operation that moves a ref writes through `Repository::ref_store`: commit, switch, branch, tag, merge ([`integrate_branch`](rustdoc:grit_lib::merge::Merge::integrate_into_branch)), reset, stash, pick, revert and rebase. So does the history engine: planning and [`History::apply`](rustdoc:grit_lib::revwalk::history::History::apply) (reword, drop, squash, move, fixup, absorb), [`History::start_rebase`](rustdoc:grit_lib::revwalk::history::History::start_rebase), `rebase --exec` and [`History::undo_last`](rustdoc:grit_lib::revwalk::history::History::undo_last). Each one reads `HEAD` with [`References::head`](rustdoc:grit_lib::refs::References::head), updates refs and `HEAD` in the store, and appends reflog entries there. With an injected store such as `MemoryRefStore`, nothing is written to `.git/HEAD`, `.git/refs`, `.git/logs` or `packed-refs`. The reflog handle, [`repo.reflogs()`](rustdoc:grit_lib::repo_handles::RepositoryReflogs), lists, reads, deletes and expires entries in that same store, and `gc` expires reflogs there too. The index, objects and operation-state files (`MERGE_HEAD`, `ORIG_HEAD`, `rebase-merge/`, sequencer state, the `grit/history-last` undo record) stay in the git directory.

The network operations keep the local side in the store the same way. [`Remote::fetch`](rustdoc:grit_lib::remote::Remote::fetch) (local, `file://`, bundle and wire remotes) reads the previous tracking-ref values from it and writes the new tips, `--prune` deletions and `refs/remotes/<name>/HEAD` there. [`Remote::push`](rustdoc:grit_lib::remote::Remote::push) records the pushed tips as tracking refs there. [`Remotes::prune`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::prune), [`set_head`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::set_head) and [`inspect`](rustdoc:grit_lib::repo_handles::RepositoryRemotes::inspect) read and write the store too. Received objects go to the repository's object store (the custom primary when one is configured). `FETCH_HEAD` is still written under the git directory, and the destination of a local push keeps its own on-disk refs. Packing the imported refs into `packed-refs` after a clone only happens on a files repository.

Reads go through the store too. [`RevWalk`](rustdoc:grit_lib::revwalk::RevWalk) ref selections (`All`, `Branches`, `Tags`, `Remotes`, with or without a glob such as `v*`), `--stdin` pseudo-options ([`RevWalk::parse_stdin_specs`](rustdoc:grit_lib::revwalk::RevWalk::parse_stdin_specs)), [`RevWalk::tag_targets`](rustdoc:grit_lib::revwalk::RevWalk::tag_targets), `:/text` searches, `@{-N}` and the other rev-parse forms that read `HEAD` all list and resolve refs through `Repository::ref_store`. Linked worktree add/list/remove do as well (see [Worktree](../worktree/)).

To advance a checked-out branch from your own code, call [`References::update`](rustdoc:grit_lib::refs::References::update) with `HEAD` or the branch name. It follows symbolic `HEAD` to the branch, appends reflog entries when [`core.logAllRefUpdates`](rustdoc:grit_lib::refs::LogRefsConfig) allows, and mirrors a `HEAD` log line when you update the checked-out branch by name. Use [`References::update_direct`](rustdoc:grit_lib::refs::References::update_direct) to detach or rewrite the `HEAD` file itself, and [`References::compare_and_swap`](rustdoc:grit_lib::refs::References::compare_and_swap) when the tip must still match an expected commit.

[`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store) remains the typed path for porcelain commits: it checks that `HEAD` points at the branch, compares the tip with the expected old commit, then moves the branch and appends the branch and `HEAD` reflog entries. On the files backend, the branch lock is held while the reflogs are appended. On every other backend, the branch update and its reflog entries commit as one transaction.

## Backend selection

On-disk layout is chosen once from **repository-local** config (`extensions.refStorage`), never from global or system config. [`RefStorageFormat`](rustdoc:grit_lib::refs::ref_storage::RefStorageFormat) in [`ref_storage`](rustdoc:grit_lib::refs::ref_storage) reads that value via `detect` and returns `files` (loose refs plus `packed-refs`) or `reftable`. [`RepoCaches::open_ref_store`](rustdoc:grit_lib::repo::caches::RepoCaches::open_ref_store) constructs [`FilesRefStore`](rustdoc:grit_lib::refs::store::FilesRefStore) or [`ReftableRefStore`](rustdoc:grit_lib::refs::store::ReftableRefStore). The store's `format()` reports which backend is active (`files`, `reftable`, or `memory` for injected stores).

### Reftable HEAD and linked worktrees

Reftable stores both symbolic and detached `HEAD` values in the worktree's own stack.
The common stack holds shared branches. The loose `HEAD` file serves repository
discovery only and does not override a stack record. Fresh reftable repositories
and linked worktrees initialize this layout before checkout, so system Git sees
the same branch and commit as `Repository::ref_store()`. Compaction also uses stack
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

When `.git/index.lock` is still held after the budget, [`stage`](rustdoc:grit_lib::index::IndexFile::stage_for_commit) and [`IndexLock::acquire`](rustdoc:grit_lib::index::IndexLock::acquire) return [`Error::LockHeld`](rustdoc:grit_lib::error::Error::LockHeld) with the lock `path`. The index is unchanged. Low-level loose-ref helpers use the same variant, while ref-store transactions retain `RefStoreError::LockHeld { name }`. Grit-owned index locks write a pid sidecar next to the lock (`index~pid.lock`) by default ([`grit_lockfile_pid_enabled`](rustdoc:grit_lib::lockfile::grit_lockfile_pid_enabled); set `core.lockfilePid=false` to opt out). The sidecar records the holder as a [`LockHolder`](rustdoc:grit_lib::lockfile::LockHolder); its `running` field records whether that process existed when checked. Converting [`LockError::Held`](rustdoc:grit_lib::lockfile::LockError::Held) with `?` preserves these fields; other `LockError` variants retain their I/O source. [`LockHolder::remove_if_stale`](rustdoc:grit_lib::lockfile::LockHolder::remove_if_stale) removes the lock and sidecar when the holder has exited (used during interrupted rewrite recovery; ordinary acquire never steals a foreign lock).

For callers migrating from `Error::IndexLocked`, match `Error::LockHeld(LockHeldError { path, holder, .. })` instead of `{ lock_path, holder }`, and import `LockHolder` from `grit_lib::lockfile` instead of `grit_lib::error`. [`LockHolder::read`](rustdoc:grit_lib::lockfile::LockHolder::read) replaces the removed prose helper `format_index_lock_blocked_detail` when inspecting a pid sidecar directly. [`pid_path_for_lock`](rustdoc:grit_lib::lockfile::pid_path_for_lock) names the sidecar path for a lock file.

```rust
use grit_lib::error::Error;
use grit_lib::lockfile::LockHolder;

fn explain(err: &Error) -> Option<String> {
    let Error::LockHeld(held) = err else {
        return None;
    };
    let path = &held.path;
    Some(match held.holder {
        Some(LockHolder { running: false, pid, .. }) => {
            format!("{} is stale (process {pid} exited); remove it", path.display())
        }
        Some(LockHolder { pid, .. }) => format!("process {pid} holds {}", path.display()),
        None => format!("another process holds {}", path.display()),
    })
}
```

Ref-store transactions follow the same budget. [`RefStore::prepare`](rustdoc:grit_lib::refs::store::RefStore::prepare) fails fast with `RefStoreError::LockHeld { name }` when the caller uses [`LockWait::Immediately`](rustdoc:grit_lib::lockfile::LockWait::Immediately) and another writer holds a lock it needs; `name` is always one of the failed transaction's own refs. On the files backend that is the per-ref `<ref>.lock` (or `packed-refs.lock` when the batch rewrites packed refs), so updates to disjoint refs run concurrently and only the same ref contends. A store with one in-process writer slot for everything (such as [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore), or a database with a single write transaction) must wait inside [`RefStore::prepare_for`](rustdoc:grit_lib::refs::store::RefStore::prepare_for) using [`SingleWriterGate`](rustdoc:grit_lib::refs::store::SingleWriterGate) (or equivalent FIFO queueing) for the supplied [`LockWait`](rustdoc:grit_lib::lockfile::LockWait); returning immediate `LockHeld` for every overlap and relying on jittered exponential backoff between retries will starve waiters under load. grit-lib's ref writes call `prepare_for` through [`prepare_with_lock_wait`](rustdoc:grit_lib::refs::prepare_with_lock_wait) with `core.filesRefLockTimeout` (100 ms by default, `0` to fail fast, `-1` to wait indefinitely) and only retry backoff when the store still reports `LockHeld` after that wait (typically another process holding a ref lock file). Immediate `LockHeld` from another process is expected; in-process single-slot stores should not surface it until the budget elapses. [`RefStore::ref_lock_wait`](rustdoc:grit_lib::refs::store::RefStore::ref_lock_wait) selects that budget: the files backend reads its git directory, a bare [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore) uses Git's default, and the store from [`Repository::ref_store`](rustdoc:grit_lib::repo::Repository::ref_store) (including an injected backend) reads `core.filesRefLockTimeout` from the open repository's git directory so porcelain and library commit helpers honor the same setting as on disk. Time spent inside `prepare_for` counts against the budget, so backends that already wait for their own lock files are not waited for twice when they ignore the passed `wait`. Call `prepare_with_lock_wait` yourself when you prepare transactions directly:

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

Batch ref updates use [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction): queue [`RefUpdate`](rustdoc:grit_lib::refs::store::RefUpdate) entries with [`Expected`](rustdoc:grit_lib::refs::store::Expected) old-value checks, optional [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate), then `RefStore::prepare` → commit or abort. Fetch, push, receive-pack, and [`update_refs`](rustdoc:grit_lib::refs::gc::update_refs) route through this path so locking and compare-and-swap semantics stay in the backend. A failed compare-and-swap on the files backend is side-effect free: neither the ref nor its reflog is updated.

Non-atomic receive-pack and local push batch ref updates the way [`References::apply_each`](rustdoc:grit_lib::refs::References::apply_each) does: deletions commit in one transaction, then creates and updates in another (matching Git `execute_commands_non_atomic`). Within each phase, [`RefTransaction::allowing_failures`](rustdoc:grit_lib::refs::store::RefTransaction::allowing_failures) records compare-and-swap, directory/file, and object-target rejections per ref while siblings still share one reftable table or one files batch. Atomic pushes still use all-or-nothing [`commit_ref_updates_on_store`](rustdoc:grit_lib::refs::commit_ref_updates_on_store), which [`References::apply`](rustdoc:grit_lib::refs::References::apply) calls on the repository's store.

To fill a new repository, such as the refs a clone imports, mark the batch with [`RefTransaction::initial`](rustdoc:grit_lib::refs::store::RefTransaction::initial), the equivalent of Git's initial ref transaction. When every update creates a direct ref that does not exist yet (no reflog, `Expected::Any` or `Expected::Missing`), the files backend checks names against one listing of the store and writes the whole batch as a single sorted `packed-refs` rewrite, with peel lines for annotated tags, under `packed-refs.lock` only. It writes no loose file or lock per ref, so 200,000 refs cost one file write. Any other batch, and every other backend, commits as usual. Only use it where no other writer creates the same refs at the same time.

Every backend (files, reftable, memory, or your own wrapper around one) refuses the same malformed batches in `prepare`, before taking any lock. A name that is not safe to store (path traversal, a `.lock` suffix, spaces, empty or doubled components, a non-`refs/` name that is not an all-caps pseudo-ref like `ORIG_HEAD`), on an update or on either side of a reflog or case-only rename, fails with [`RefStoreError::InvalidRefName`](rustdoc:grit_lib::refs::store::RefStoreError::InvalidRefName). A symbolic value whose target is malformed (control characters, spaces, `..`, `.lock`, and the other rules Git applies to symref targets), would make `HEAD` point outside `refs/`, or points a ref at itself, fails with [`RefStoreError::InvalidSymrefTarget`](rustdoc:grit_lib::refs::store::RefStoreError::InvalidSymrefTarget). Non-`HEAD` symrefs may use Git one-level targets such as `foo` or `heads/main`; `HEAD` must point under `refs/`. The target ref does not have to exist yet (unborn branches and aliases to not-yet-created refs stay valid). Storing the all-zero object id as a direct value fails with [`RefStoreError::NullOid`](rustdoc:grit_lib::refs::store::RefStoreError::NullOid), because Git reads such a ref as broken. To delete a ref, set `new_value: None`; the commit removes that ref's reflog (matching Git), even when the update carries a [`ReflogUpdate`](rustdoc:grit_lib::refs::store::ReflogUpdate). Reflog-only updates may still record the null id.

The store a repository hands out, [`Repository::ref_store`](rustdoc:grit_lib::repo::Repository::ref_store), also checks every transaction against the repository's objects before any lock, as `git update-ref` does. This covers injected stores, [`References::update`](rustdoc:grit_lib::refs::References::update) and the branch move of [`update_branch_for_commit_on_store`](rustdoc:grit_lib::refs::update_branch_for_commit_on_store) (its files-backend fast path runs the same check before it takes the branch lock), and every other `References` writer (`update_direct`, `compare_and_swap`, `apply`, `apply_each`). A direct value naming an object the repository does not store fails with [`RefStoreError::MissingObject`](rustdoc:grit_lib::refs::store::RefStoreError::MissingObject). A branch (`refs/heads/…`) or `HEAD` pointed at anything but a commit fails with [`RefStoreError::NotACommit`](rustdoc:grit_lib::refs::store::RefStoreError::NotACommit). Tags, remote-tracking refs and other refs may name any object. The lookup reads object headers only and never lazily fetches from a promisor remote. The empty tree counts only once it is actually stored, because `git fsck` reports a ref to an unstored one as missing. An unreadable object fails with [`RefStoreError::ObjectLookup`](rustdoc:grit_lib::refs::store::RefStoreError::ObjectLookup). Deletes and symbolic values are not looked up; reflog-only updates and explicit reflog object ids are checked the same way. One bad update fails the whole batch, and every ref stays unchanged.

Because of this check, writing a commit and then moving a branch to it with a compare-and-swap update is safe against a concurrent `git gc --prune=now`. If the prune removes the commit before the update, the update fails with `MissingObject` and does not leave a branch pointing at a missing commit. To write a dangling ref on purpose (for example, a corrupt-repository fixture), set [`RefUpdateFlags::skip_oid_verification`](rustdoc:grit_lib::refs::store::RefUpdateFlags::skip_oid_verification) on the update. A backend opened on its own, such as one from [`RepoCaches::open_ref_store`](rustdoc:grit_lib::repo::caches::RepoCaches::open_ref_store) or a bare `MemoryRefStore`, does not look at objects.

## Reset validation

[`reset_to_commit`](rustdoc:grit_lib::porcelain::reset::ResetOutcome) validates
[`ResetOptions::identity`](rustdoc:grit_lib::porcelain::reset::ResetOptions::identity)
before changing any repository state. If a caller modifies a public
[`Signature`](rustdoc:grit_lib::ident::Signature) field to an invalid value, reset
returns [`ObjectError::InvalidSignature`](rustdoc:grit_lib::error::ObjectError::InvalidSignature)
with HEAD, branch refs, index bytes, worktree files, reflogs, and operation state
unchanged. This includes merge autostashes and `refs/stash`.

The same validation applies to
[`preview_reset`](rustdoc:grit_lib::porcelain::reset::ResetOutcome) and dry runs,
even when the target is already HEAD or `core.logAllRefUpdates=false`. This is a
preflight guarantee for invalid signatures; later I/O errors can leave partial
updates.

## Resolving HEAD and symbolic refs

`resolve_head` reads `HEAD` from a git directory and returns a `HeadState`; `resolve_head_for_repository` reads it through the repository's store (an empty injected store gives `HeadState::Invalid`). When you only need the commit `HEAD` is on, call [`Repository::head_commit`](rustdoc:grit_lib::repo::Repository::head_commit): it reads through the same store and fails with [`RefError::UnbornHead`](rustdoc:grit_lib::error::RefError::UnbornHead) on a branch with no commits yet, or [`RefError::InvalidHead`](rustdoc:grit_lib::error::RefError::InvalidHead) when `HEAD` names nothing (both under [`Error::Ref`](rustdoc:grit_lib::error::Error::Ref)). For the tree to diff or check out against, [`Repository::head_tree`](rustdoc:grit_lib::repo::Repository::head_tree) returns `None` on an unborn branch and the same `RefError::InvalidHead` otherwise; [`HeadState::tree`](rustdoc:grit_lib::state::HeadState::tree) does the same for a `HeadState` you already resolved, but treats an invalid `HEAD` as "no tree". `resolve_ref` (or `RefStore::resolve` on the repository store) follows symbolic refs with cycle detection. `read_ref_file` returns a `Ref` without resolving the full chain.

On every backend, a missing ref fails with [`RefStoreError::NotFound`](rustdoc:grit_lib::refs::store::RefStoreError::NotFound) inside `Error::RefStore`, naming the ref that is absent. For a dangling symbolic ref (an unborn branch behind `HEAD`, say) that is the target, not the name you asked for. [`References::delete`](rustdoc:grit_lib::refs::References::delete) on a missing ref fails the same way. Match on the variant rather than the message:

```rust
use grit_lib::error::Error;
use grit_lib::refs::store::{RefName, RefStoreError};

let topic = RefName::try_from("refs/heads/topic")?;
match repo.references().resolve(&topic) {
    Ok(oid) => println!("topic is at {oid}"),
    Err(Error::RefStore(RefStoreError::NotFound { name })) => {
        println!("{} does not exist", name.as_str());
    }
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
`Repository::ref_store` (`for_each_reflog_ref`, `for_each_reflog_entry`, `replace_reflog`, `rewrite_reflog`),
so it behaves the same on the files, reftable and injected backends; on the files
backend, newest-first queries read the log backwards and stop at the limit.

[`References::list`](rustdoc:grit_lib::refs::References::list) iterates with a prefix and [`References::list_glob`](rustdoc:grit_lib::refs::References::list_glob) with a glob such as `refs/tags/v*`, both sorted by name. Use [`References::apply`](rustdoc:grit_lib::refs::References::apply) (or a [`RefTransaction`](rustdoc:grit_lib::refs::store::RefTransaction) on the store) when updating many refs atomically. There are no git-directory-path ref functions: open a [`Repository`](rustdoc:grit_lib::repo::Repository) on the git directory and use its handles.

Reflog helpers live in [`reflog`](rustdoc:grit_lib::refs::reflog) and on [`RefStore`](rustdoc:grit_lib::refs::store::RefStore) (`for_each_reflog_entry`, `replace_reflog`, [`rewrite_reflog`](rustdoc:grit_lib::refs::store::RefStore::rewrite_reflog), …). Use `rewrite_reflog` for expire, delete, and other read-modify-write paths. Built-in backends hold the same writer lock as `prepare`/`commit` for the whole call; **custom backends must implement `rewrite_reflog` themselves** (there is no default) and keep that exclusion through the transform closure—do not call back into the store from the closure. [`RepositoryReflogs::append`](rustdoc:grit_lib::repo_handles::RepositoryReflogs::append) records an entry without moving the ref, [`RepositoryReflogs::exists`](rustdoc:grit_lib::repo_handles::RepositoryReflogs::exists) checks for a log, and [`References::reflog`](rustdoc:grit_lib::refs::References::reflog) reads entries back oldest first.

## Operation state

[`repo.operation()`](rustdoc:grit_lib::operation::Operations) reports what a paused merge, rebase, `am`, cherry-pick, revert or bisect left behind. [`state`](rustdoc:grit_lib::operation::Operations::state) returns an [`OperationState`](rustdoc:grit_lib::operation::OperationState) with the paused operations, `MERGE_HEAD`, `MERGE_MSG`, `CHERRY_PICK_HEAD`, `REVERT_HEAD` and `ORIG_HEAD`; [`in_progress`](rustdoc:grit_lib::operation::Operations::in_progress) is the cheap check, [`blocking_switch`](rustdoc:grit_lib::operation::Operations::blocking_switch) names the operation that must finish before `HEAD` may move, [`status_state`](rustdoc:grit_lib::operation::Operations::status_state) has the details `grit status` shows, and [`clear`](rustdoc:grit_lib::operation::Operations::clear) forgets every paused operation without touching `HEAD`, refs or files.

```rust
let state = repo.operation().state()?;
if !state.merge_heads.is_empty() {
    println!("merging {} commit(s)", state.merge_heads.len());
}
if let Some(op) = repo.operation().blocking_switch()? {
    println!("finish the {} first", op.description());
}
```

## Branches and tags

[`repo.branches()`](rustdoc:grit_lib::repo_handles::Branches) covers branch create, delete, rename, upstream configuration, and listing. Pass [`BranchListQuery`](rustdoc:grit_lib::refs::branch_tracking::BranchListQuery) and [`BranchDetail::Full`](rustdoc:grit_lib::refs::branch_tracking::BranchDetail::Full) to [`Branches::list`](rustdoc:grit_lib::repo_handles::Branches::list) for what `grit branch` renders (subject, committer time, upstream ahead/behind, worktree). Use [`BranchDetail::Names`](rustdoc:grit_lib::refs::branch_tracking::BranchDetail::Names) for ref names and oids only. For a minimal local list without commit reads, [`Branches::list_simple`](rustdoc:grit_lib::repo_handles::Branches::list_simple) returns [`BranchList`](rustdoc:grit_lib::porcelain::branch::BranchList) (name, oid, current).

[`repo.tags()`](rustdoc:grit_lib::repo_handles::Tags) creates, lists, deletes, and verifies tags. Call [`Tags::list`](rustdoc:grit_lib::repo_handles::Tags::list) with [`TagListOptions`](rustdoc:grit_lib::repo_handles::TagListOptions) and [`TagDetail::Names`](rustdoc:grit_lib::porcelain::tag::TagDetail::Names) for names without tag-object reads, [`TagDetail::Kind`](rustdoc:grit_lib::porcelain::tag::TagDetail::Kind) for packed-peel classification, or [`TagDetail::Annotation`](rustdoc:grit_lib::porcelain::tag::TagDetail::Annotation) for complete [`TagSummary`](rustdoc:grit_lib::porcelain::tag::TagSummary) rows (annotation messages and taggers). [`TagListOptions::full_annotations`](rustdoc:grit_lib::repo_handles::TagListOptions::full_annotations) is a convenience builder for the legacy annotated listing.

[`repo.checkout()`](rustdoc:grit_lib::repo_handles::Checkout) switches branches, checks out trees, resets, restores paths, cleans untracked files, and performs index path moves — the same operations the `grit switch`, `reset`, `restore`, `clean`, `mv`, and `rm` commands call.

[`Branches::delete`](rustdoc:grit_lib::repo_handles::Branches::delete) refuses the current branch and branches checked out in another worktree. With [`BranchDeleteMode::Safe`](rustdoc:grit_lib::porcelain::branch::BranchDeleteMode::Safe) it also requires the branch tip to be reachable from the branch's upstream, or from `HEAD` when there is no upstream (the same rule as `git branch -d`).

Each refusal is an `Error::Branch(BranchError::...)`: `NotFound` for a missing branch, `CurrentlyCheckedOut { name }` for the branch named by this repository's own `HEAD`, `CheckedOut { name, worktree }` for a branch held by another worktree, or `NotFullyMerged` for an unmerged tip.

The current-branch check reads `HEAD` through the repository's store, including injected stores, even when the on-disk `HEAD` differs. `DeleteMode::Force` skips only the merge-state check; both it and dry runs retain the checked-out safety checks and leave HEAD and refs unchanged on refusal. The CLI maps both checked-out variants to the JSON kind `branch_checked_out`.

[`Tags::delete`](rustdoc:grit_lib::repo_handles::Tags::delete) returns the object the tag pointed at, or `Error::Tag(`[`TagError::NotFound`](rustdoc:grit_lib::porcelain::tag::TagError::NotFound)`)` when the tag does not exist. Both deletes run on the repository's ref store (so an injected store is honoured), compare-and-swap against the commit they checked, and remove the ref's reflog.

```rust
use grit_lib::error::Error;
use grit_lib::porcelain::branch::{BranchDeleteMode, BranchDeleteOptions, BranchError};

let dry_run = false;
match repo.branches().delete(
    "topic",
    &BranchDeleteOptions {
        mode: BranchDeleteMode::Safe,
        dry_run,
    },
) {
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

[`References::query`](rustdoc:grit_lib::refs::References::query) shares ancestry results between evaluated tips for finite, name-ordered `contains` queries. It stops after `RefQuery::limit` matching rows, preserving patterns, namespace unions, reverse ordering, and annotated-tag peeling. Filter targets are validated even for a zero limit or an empty candidate set. Unlimited queries, version/date sorts, and combined reachability or object filters retain the shared batch walk.

## Upstream, target branch, and ahead/behind

[`branch_tracking`](rustdoc:grit_lib::refs::branch_tracking) answers "how does this branch compare with its base?", the question `grit status` and `grit shortlog` show at the top. `upstream_branch` resolves a branch's configured upstream, `target_branch` finds the repository's trunk (the `target.branch` config value, then [`DEFAULT_TARGET_CANDIDATES`](rustdoc:grit_lib::refs::branch_tracking::DEFAULT_TARGET_CANDIDATES)), and `comparison_base` picks the upstream when it exists and the target otherwise (legacy helper). [`BranchStanding::compute`](rustdoc:grit_lib::refs::branch_tracking::BranchStanding::compute) returns separate [`TrackedComparison`](rustdoc:grit_lib::refs::branch_tracking::TrackedComparison) values for target and upstream for status, with [`StatusHeader`](rustdoc:grit_lib::refs::branch_tracking::StatusHeader) for the human header line and pull hints. Each base resolves to a [`ComparisonBase`](rustdoc:grit_lib::refs::branch_tracking::ComparisonBase) with the display name, full ref, commit, and [`BaseSource`](rustdoc:grit_lib::refs::branch_tracking::BaseSource), through the repository's ref store.

[`TipComparison`](rustdoc:grit_lib::refs::branch_tracking::TipComparison)`::compute` then counts commits on each side (the same numbers as `git rev-list --left-right --count tip...base`), keeps the newest ahead commits up to a limit, and classifies the result as a [`Standing`](rustdoc:grit_lib::refs::branch_tracking::Standing): even, ahead, behind, or diverged. For `grit shortlog`, [`Branches::compare_with_target`](rustdoc:grit_lib::repo_handles::Branches::compare_with_target) with [`TargetComparisonOptions`](rustdoc:grit_lib::refs::branch_tracking::TargetComparisonOptions) resolves the current branch and target (including `--target`) and returns [`TargetComparison`](rustdoc:grit_lib::refs::branch_tracking::TargetComparison) with bounded ahead and optional behind OID lists. For symmetric tip comparisons when you already have two commit ids, use `TargetTipListOptions` and `TargetTipLists``::compute`.

```rust
use grit_lib::refs::branch_tracking::{BranchStanding, Standing, StandingOptions};
use grit_lib::state::HeadState;

let head = repo.references().head()?;
let standing = BranchStanding::compute(&repo, &head, StandingOptions::default())?;
if let Some(target) = standing.target.as_ref() {
    if target.standing() == Standing::Behind {
        println!("{} new commits on {}", target.behind, target.base.name);
    }
}
```

## Writing a ref backend

Ref backend design, transaction helpers, conformance tests, and how init interacts with [`RepositoryOptions::with_ref_store`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_ref_store) are documented in [Writing storage backends](storage-backends.md) (see [Ref storage](storage-backends.md#ref-storage) and [Testing](storage-backends.md#testing)). The worked example remains [`grit-lib/tests/refstore_custom_backend.rs`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/tests/refstore_custom_backend.rs).

## Ref names (`RefName` / `RefNamePolicy`)

Parse human-facing ref strings with [`RefName::parse`](rustdoc:grit_lib::refs::check_ref_format::RefName::parse) and a [`RefNamePolicy`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy). The default policy matches `git check-ref-format` with no flags. Setters such as [`allow_onelevel`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::allow_onelevel), [`refspec_pattern`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::refspec_pattern), [`normalize`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::normalize), [`storable`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::storable), [`advertised_fetch`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::advertised_fetch), and [`receive_pack`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::receive_pack) mirror the free validators removed from the public API.

Branch and tag short names use [`RefName::branch`](rustdoc:grit_lib::refs::check_ref_format::RefName::branch) and [`RefName::tag`](rustdoc:grit_lib::refs::check_ref_format::RefName::tag). When normalization may allocate, parse into an owned [`RefName`](rustdoc:grit_lib::refs::check_ref_format::RefName); when the input is already canonical, [`RefNameRef::parse`](rustdoc:grit_lib::refs::check_ref_format::RefNameRef::parse) validates in place. Names loaded from the ref store skip re-validation when the library already trusts the on-disk source.

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

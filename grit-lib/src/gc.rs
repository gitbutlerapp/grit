//! In-process maintenance primitives that embedders such as `jj` use in place
//! of shelling out to `git gc` / `git remote show` / `gix::refs::transaction`.
//!
//! These are the remaining "replaced paths" from the jj spike (PR #9632) that
//! are not transport-shaped:
//!
//! * [`prune_loose_unreachable`] — what `jj util gc` actually needs: delete the
//!   loose objects that are not reachable from a set of roots (full repack /
//!   `pack-refs` are explicitly out of scope).
//! * [`remote_default_branch_local`] — the `git remote show` default-branch
//!   lookup for a local / `file://` remote, via the remote `HEAD` symref.
//! * [`update_refs`] — a thin, compare-and-swap, all-or-nothing batch ref
//!   transaction over the [`crate::refs`] primitives (the in-process equivalent
//!   of what jj built on `gix::refs::transaction`).
//!
//! Scope note: only the local / on-disk path is in scope here. `git://`,
//! `http(s)`, and `ssh` remotes are out of scope and left as TODOs.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::refs::{self, Ref};

/// Result of a loose-object prune.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Number of loose objects deleted.
    pub pruned: usize,
    /// Number of loose objects kept (reachable, or too recent to prune).
    pub kept: usize,
}

/// Delete loose objects in `odb` that are not reachable from `reachable_roots`.
///
/// This is the in-process core of `jj util gc`: it walks the full reachability
/// closure from `reachable_roots` (commits → parents and trees, trees → entries,
/// annotated tags → their target) and then removes every **loose** object whose
/// id is not in that closure. Packed objects are never touched — only loose
/// object files under `objects/??/` are candidates for deletion.
///
/// When `keep_newer_than` is `Some(t)`, a loose object is only deleted if its
/// file modification time is strictly older than `t`. This mirrors Git's
/// `gc.pruneExpire` grace window: recently written objects (which may be the
/// in-progress target of a concurrent operation) are kept even when currently
/// unreachable. A `None` grace window prunes every unreachable loose object
/// regardless of age.
///
/// Submodule (gitlink) tree entries are skipped during the walk: the commit they
/// name lives in another object store.
///
/// The repository hash width is threaded through [`Odb::hash_algo`] (via the
/// 2-char fan-out directory + suffix length), so SHA-256 repositories work.
///
/// # Errors
///
/// Returns an error if a root or a reachable object cannot be read or parsed, or
/// on I/O failure while enumerating or deleting loose object files.
pub fn prune_loose_unreachable(
    odb: &Odb,
    reachable_roots: &[ObjectId],
    keep_newer_than: Option<SystemTime>,
) -> Result<PruneStats> {
    odb.require_files_primary("gc")?;
    // 1. Full reachability closure from the roots.
    let reachable = reachable_closure(odb, reachable_roots)?;

    // 2. Enumerate loose objects and delete the unreachable, sufficiently-old ones.
    let mut stats = PruneStats::default();
    for (oid, path) in odb.enumerate_local_loose_objects()? {
        if reachable.contains(&oid) {
            stats.kept += 1;
            continue;
        }

        // Respect the grace window: keep objects whose mtime is not strictly
        // older than the cutoff.
        if let Some(cutoff) = keep_newer_than {
            let too_new = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|mtime| mtime >= cutoff)
                .unwrap_or(false);
            if too_new {
                stats.kept += 1;
                continue;
            }
        }

        match std::fs::remove_file(&path) {
            Ok(()) => stats.pruned += 1,
            // A concurrent prune may have already removed it; treat as pruned.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => stats.pruned += 1,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(stats)
}

/// Object ids named directly by refs (any kind), used as roots for [`prune_loose_unreachable`].
///
/// Includes annotated tag objects, blob/tree tips, and detached-HEAD oids. Does not peel tags to
/// commits only — the reachability walk expands each root through the object graph.
///
/// # Errors
///
/// Propagates ref listing or HEAD read failures.
pub fn collect_referenced_object_roots(git_dir: &Path) -> Result<Vec<ObjectId>> {
    let mut roots = HashSet::new();
    for (_, oid) in refs::list_refs(git_dir, "refs/")? {
        roots.insert(oid);
    }
    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        match refs::read_ref_file(&head_path)? {
            Ref::Direct(oid) => {
                roots.insert(oid);
            }
            Ref::Symbolic(_) => {}
        }
    }
    Ok(roots.into_iter().collect())
}

/// Compute the full object closure reachable from `roots` (commits → parents and
/// tree, trees → entries, tags → target). Mirrors the reachability walk used by
/// the transfer pack builder, but specialized for prune (no exclusion set).
fn reachable_closure(odb: &Odb, roots: &[ObjectId]) -> Result<HashSet<ObjectId>> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    for &root in roots {
        if seen.insert(root) {
            queue.push_back(root);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                for parent in commit.parents {
                    if seen.insert(parent) {
                        queue.push_back(parent);
                    }
                }
                if seen.insert(commit.tree) {
                    queue.push_back(commit.tree);
                }
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    // Skip submodule (gitlink) entries.
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if seen.insert(entry.oid) {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                if seen.insert(tag.object) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }

    Ok(seen)
}

/// Enumerate the loose objects physically present in `odb`'s objects directory,
/// returning each `(oid, path)`. Only the `??/<rest>` fan-out directories are
/// scanned; pack files, `info/`, and any non-object entries are ignored.
///
/// Entries whose names do not form a valid full-length hex OID for the
/// repository's hash algorithm are skipped (e.g. tmp files, the wrong hash
/// width), matching Git's loose-object scan.
/// Return the short name of a local remote's default branch (its `HEAD` symref
/// target), e.g. `main` for a `HEAD` pointing at `refs/heads/main`.
///
/// This is the `git remote show <remote>` default-branch lookup for a local /
/// `file://` remote. It reuses [`crate::remote::list_refs_from_git_dir`]'s symref handling
/// to read the remote `HEAD` and strips the `refs/heads/` prefix from its
/// target. Returns `None` when the remote has no symbolic `HEAD` (e.g. a
/// detached or absent `HEAD`).
///
/// # Errors
///
/// Returns an error if the remote git directory cannot be read.
//
// TODO(phase: remote transports): the `git://`, `http(s)`, and `ssh`
// default-branch lookup (handshake `symref=HEAD:`) is out of scope here.
pub fn remote_default_branch_local(remote_git_dir: &Path) -> Result<Option<String>> {
    let remote_odb =
        Odb::new(&remote_git_dir.join("objects")).with_config_git_dir(remote_git_dir.to_path_buf());

    let entries = crate::remote::list_refs_from_git_dir(
        remote_git_dir,
        &remote_odb,
        &crate::remote::ListRefsOptions {
            symrefs: true,
            ..Default::default()
        },
    )
    .map_err(crate::error::Error::from)?;

    for entry in &entries {
        if entry.name == "HEAD" {
            return Ok(entry
                .symref_target
                .as_ref()
                .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned()));
        }
    }

    Ok(None)
}

/// A single ref change in an [`update_refs`] batch transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefTransactionItem {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: String,
    /// New value to write, or `None` to delete the ref.
    pub new_oid: Option<ObjectId>,
    /// Compare-and-swap expectation. When `Some`, the ref's current value must
    /// equal this for the item to apply (an `expected_old` of an oid that is not
    /// the current value — including when the ref is absent — fails the batch).
    /// When `None`, the current value is not checked.
    ///
    /// Note: this CAS form expects the ref to currently hold `expected_old`. To
    /// require that a ref be *created* (must not already exist), leave this
    /// `None` — callers that need create-only semantics check existence
    /// themselves; matching jj's transaction model where `None` means "any".
    pub expected_old: Option<ObjectId>,
}

/// Apply a batch of ref create/update/delete operations transactionally with
/// compare-and-swap semantics.
///
/// Every item whose `expected_old` is `Some` is checked against the ref's
/// current value first; if **any** CAS check fails, the entire batch is rejected
/// and **nothing** is written (all-or-nothing). Only once all CAS checks pass
/// are the changes applied:
///
/// * `new_oid = Some(oid)` writes (creates or updates) the ref to `oid` via
///   [`crate::refs::write_ref`].
/// * `new_oid = None` deletes the ref via [`crate::refs::delete_ref`].
///
/// This is a thin transactional wrapper over the [`crate::refs`] primitives,
/// matching the in-process ref-update path jj built on `gix::refs::transaction`.
///
/// # Errors
///
/// Returns [`Error::Message`] describing the first failing CAS check (before any
/// mutation), or an I/O / ref error if applying a change fails after the checks
/// passed. The CAS pre-check makes a clean rejection the common failure mode;
/// an apply-time error can still leave a partially-applied batch (the
/// pre-checked, conflict-free case), which is the same guarantee Git's files
/// backend gives outside of `core.refTransaction` hooks.
pub fn update_refs(git_dir: &Path, updates: &[RefTransactionItem]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut seen = HashSet::new();
    for item in updates {
        if !seen.insert(&item.name) {
            return Err(Error::Message(format!(
                "ref transaction rejected: multiple updates for ref '{}' not allowed",
                item.name
            )));
        }
    }

    // Phase 1: verify every CAS expectation against current state. Apply nothing
    // if any check fails.
    for item in updates {
        if let Some(expected) = item.expected_old {
            let current = crate::refs::resolve_ref(git_dir, &item.name).ok();
            if current != Some(expected) {
                return Err(Error::Message(format!(
                    "ref transaction rejected: '{}' expected {} but found {}",
                    item.name,
                    expected,
                    current
                        .map(|o| o.to_hex())
                        .unwrap_or_else(|| "<absent>".to_owned()),
                )));
            }
        }
    }

    let batch: Vec<crate::refs::RefBatchItem> = updates
        .iter()
        .map(|item| crate::refs::RefBatchItem {
            name: item.name.clone(),
            new_oid: item.new_oid,
        })
        .collect();
    if let Err(unavail) = crate::refs::verify_ref_transaction_batch(git_dir, &batch) {
        let refname = match &unavail {
            crate::refs::RefnameUnavailable::AncestorExists { new_ref, .. }
            | crate::refs::RefnameUnavailable::DescendantExists { new_ref, .. } => new_ref.clone(),
            crate::refs::RefnameUnavailable::SameBatch { refname, .. } => refname.clone(),
        };
        return Err(Error::Message(format!(
            "ref transaction rejected: cannot lock ref '{refname}': {}",
            unavail.lock_message_suffix()
        )));
    }

    if crate::reftable::is_reftable_repo(git_dir) {
        let rt_updates: Vec<crate::reftable::ReftableTransactionUpdate> = updates
            .iter()
            .map(|item| crate::reftable::ReftableTransactionUpdate {
                refname: item.name.clone(),
                value: Some(match item.new_oid {
                    Some(oid) => crate::reftable::RefValue::Val1(oid),
                    None => crate::reftable::RefValue::Deletion,
                }),
                log: None,
                expected_old: item.expected_old,
            })
            .collect();
        return crate::reftable::reftable_write_transaction(git_dir, rt_updates);
    }

    // Phase 2: apply. Re-check CAS immediately before each write so concurrent
    // updaters cannot both succeed after the initial pre-check (see TESTING.md
    // gap note for whole-batch lock-all atomicity).
    for item in updates {
        if let Some(expected) = item.expected_old {
            let current = crate::refs::resolve_ref(git_dir, &item.name).ok();
            if current != Some(expected) {
                return Err(Error::Message(format!(
                    "ref transaction rejected: '{}' expected {} but found {}",
                    item.name,
                    expected,
                    current
                        .map(|o| o.to_hex())
                        .unwrap_or_else(|| "<absent>".to_owned()),
                )));
            }
        }
        match (&item.new_oid, item.expected_old) {
            (Some(oid), Some(expected)) => {
                crate::refs::write_ref_cas(git_dir, &item.name, oid, expected)?
            }
            (Some(oid), None) => crate::refs::write_ref(git_dir, &item.name, oid)?,
            (None, Some(expected)) => crate::refs::delete_ref_cas(git_dir, &item.name, expected)?,
            (None, None) => crate::refs::delete_ref(git_dir, &item.name)?,
        }
    }

    Ok(())
}

#[cfg(test)]
mod update_refs_tests {
    use super::*;
    use crate::repo::init_repository;
    use tempfile::TempDir;

    fn sample_oid(hex: &str) -> ObjectId {
        hex.parse().expect("oid")
    }

    #[test]
    fn rejects_duplicate_ref_names_in_batch() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
        let oid = sample_oid("67bf698f3ab735e92fb011a99cff3497c44d30c1");
        let items = vec![
            RefTransactionItem {
                name: "refs/heads/dup".to_owned(),
                new_oid: Some(oid),
                expected_old: None,
            },
            RefTransactionItem {
                name: "refs/heads/dup".to_owned(),
                new_oid: Some(oid),
                expected_old: None,
            },
        ];
        let err = update_refs(&repo.git_dir, &items).expect_err("duplicate");
        assert!(
            err.to_string()
                .contains("multiple updates for ref 'refs/heads/dup'"),
            "{err}"
        );
    }

    #[test]
    fn reftable_cas_rechecked_under_stack_lock() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(tmp.path(), false, "main", None, "reftable").expect("init");
        let git_dir = repo.git_dir;
        let c1 = sample_oid("67bf698f3ab735e92fb011a99cff3497c44d30c1");
        let c2 = sample_oid("1111111111111111111111111111111111111111");
        crate::refs::write_ref(&git_dir, "refs/heads/locked-cas", &c1).expect("seed");
        crate::refs::write_ref(&git_dir, "refs/heads/locked-cas", &c2).expect("advance");
        let err = update_refs(
            &git_dir,
            &[RefTransactionItem {
                name: "refs/heads/locked-cas".to_owned(),
                new_oid: Some(c1),
                expected_old: Some(c1),
            }],
        )
        .expect_err("stale cas");
        assert!(err.to_string().contains("expected"));
        assert_eq!(
            crate::refs::resolve_ref(&git_dir, "refs/heads/locked-cas").unwrap(),
            c2
        );
    }
}

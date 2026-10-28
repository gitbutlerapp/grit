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

use std::cell::OnceCell;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, ObjectId, ObjectKind, OidHashSet, TreeRefIter};
use crate::odb::Odb;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
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
/// Like `git prune`, the walk is skipped when no loose object is old enough to
/// delete; the roots themselves are still required to exist.
///
/// Submodule (gitlink) tree entries are skipped during the walk: the commit they
/// name lives in another object store.
///
/// When `odb` belongs to a repository ([`Odb::config_git_dir`]), the commits listed in
/// its `shallow` file are treated as having no parents, like every Git history walk:
/// their trees are walked but their (absent) parents are not. Promisor pack members
/// and their available descendants form another boundary: promised objects may
/// remain absent and no maintenance walk fetches them from the remote.
///
/// The repository hash width is threaded through [`Odb::hash_algo`] (via the
/// 2-char fan-out directory + suffix length), so SHA-256 repositories work.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] if a non-promised root is missing, an error if the walk runs
/// and a reachable object is missing or cannot be read or parsed, or on I/O failure
/// while enumerating or deleting loose object files.
pub fn prune_loose_unreachable(
    odb: &Odb,
    reachable_roots: &[ObjectId],
    keep_newer_than: Option<SystemTime>,
) -> Result<PruneStats> {
    prune_loose_unreachable_with_dry_run(odb, reachable_roots, keep_newer_than, false)
}

/// Like [`prune_loose_unreachable`], but skips deletion when `dry_run` is true.
///
/// # Errors
///
/// Same as [`prune_loose_unreachable`].
pub fn prune_loose_unreachable_with_dry_run(
    odb: &Odb,
    reachable_roots: &[ObjectId],
    keep_newer_than: Option<SystemTime>,
    dry_run: bool,
) -> Result<PruneStats> {
    let shallow = odb
        .config_git_dir()
        .map(crate::shallow::load_shallow_boundaries)
        .unwrap_or_default();
    prune_loose_unreachable_with(
        odb,
        &LazyReachable::new(odb, reachable_roots, &shallow),
        keep_newer_than,
        dry_run,
    )
}

/// [`prune_loose_unreachable_with_dry_run`] sharing `reachable` with the other steps of a
/// maintenance run, so one run walks the object graph at most once.
pub(crate) fn prune_loose_unreachable_with(
    odb: &Odb,
    reachable: &LazyReachable<'_>,
    keep_newer_than: Option<SystemTime>,
    dry_run: bool,
) -> Result<PruneStats> {
    odb.require_files_primary("gc")?;
    let mut stats = PruneStats::default();

    // Respect the grace window first: objects whose mtime is not strictly older than
    // the cutoff are kept whether reachable or not, so they never need the walk.
    let candidates: Vec<_> = odb
        .enumerate_local_loose_objects()?
        .into_iter()
        .filter(|(_, path)| {
            let too_new = keep_newer_than.is_some_and(|cutoff| {
                std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|mtime| mtime >= cutoff)
            });
            if too_new {
                stats.kept += 1;
            }
            !too_new
        })
        .collect();
    if candidates.is_empty() {
        reachable.require_roots_exist()?;
        return Ok(stats);
    }

    let reachable = reachable.get()?;
    for (oid, path) in candidates {
        if reachable.contains(&oid) {
            stats.kept += 1;
            continue;
        }

        if dry_run {
            stats.pruned += 1;
            continue;
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

/// Object closure reachable from a fixed set of roots, walked on first use.
///
/// One maintenance run (repack skip check, loosening superseded packs, unreachable prune)
/// shares a single instance, so the graph is walked at most once and not at all when no
/// step needs it.
pub(crate) struct LazyReachable<'a> {
    odb: &'a Odb,
    roots: &'a [ObjectId],
    shallow: &'a HashSet<ObjectId>,
    closure: OnceCell<OidHashSet>,
    promised: OnceCell<HashSet<ObjectId>>,
}

impl<'a> LazyReachable<'a> {
    /// Closure of `roots` in `odb`, not following the parents of the `shallow` boundary
    /// commits; nothing is read until [`Self::get`].
    pub(crate) fn new(odb: &'a Odb, roots: &'a [ObjectId], shallow: &'a HashSet<ObjectId>) -> Self {
        Self {
            odb,
            roots,
            shallow,
            closure: OnceCell::new(),
            promised: OnceCell::new(),
        }
    }

    /// The reachable set, walking the graph on the first call.
    ///
    /// # Errors
    ///
    /// Same as the reachability walk: [`Error::ObjectNotFound`] for a missing non-promised
    /// object, and read or parse errors.
    pub(crate) fn get(&self) -> Result<&OidHashSet> {
        if let Some(closure) = self.closure.get() {
            return Ok(closure);
        }
        let closure = reachable_closure(self.odb, self.roots, self.shallow, self.promised()?)?;
        Ok(self.closure.get_or_init(|| closure))
    }

    /// Fail with [`Error::ObjectNotFound`] when a non-promised root is missing.
    pub(crate) fn require_roots_exist(&self) -> Result<()> {
        if self.closure.get().is_some() {
            return Ok(());
        }
        match require_objects_exist(self.odb, self.roots, &HashSet::new()) {
            Err(Error::ObjectNotFound(_)) => {
                require_objects_exist(self.odb, self.roots, self.promised()?)
            }
            result => result,
        }
    }

    /// Promisor membership and available descendants, computed once without fetching.
    pub(crate) fn promised(&self) -> Result<&HashSet<ObjectId>> {
        if let Some(promised) = self.promised.get() {
            return Ok(promised);
        }
        let promised = crate::promisor::maintenance_promise_boundary(self.odb)?;
        Ok(self.promised.get_or_init(|| promised))
    }
}

/// File type bits of a tree entry mode, and the value that marks a directory.
pub(crate) const S_IFMT: u32 = 0o170000;
pub(crate) const S_IFDIR: u32 = 0o040000;

/// Compute the full object closure reachable from `roots` (commits → parents and
/// tree, trees → entries, tags → target). Mirrors the reachability walk used by
/// the transfer pack builder, but specialized for prune (no exclusion set).
///
/// Commits in `shallow` are boundaries: their tree is walked, their parents are not.
/// Blobs named by trees are checked for existence but not read: they link to nothing.
///
/// # Errors
///
/// [`Error::ObjectNotFound`] when a reachable object is missing, and read or parse
/// errors for the objects that are walked.
fn reachable_closure(
    odb: &Odb,
    roots: &[ObjectId],
    shallow: &HashSet<ObjectId>,
    promised: &HashSet<ObjectId>,
) -> Result<OidHashSet> {
    // Protect available promised loose objects too, even beyond a stopped walk.
    let mut seen: OidHashSet = promised.iter().copied().collect();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    let oid_len = odb.hash_algo().len();
    let mut blobs: Vec<ObjectId> = Vec::new();
    let alternates = crate::alternate_boundary::AlternateSources::load(odb)?;

    for &root in roots {
        if seen.insert(root) && !alternates.contains(&root) {
            queue.push_back(root);
        }
    }

    while let Some(oid) = queue.pop_front() {
        if alternates.contains(&oid) {
            continue;
        }
        let obj = odb
            .try_read_once(&oid)?
            .ok_or_else(|| Error::ObjectNotFound(oid.to_hex()))?;
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                if !shallow.contains(&oid) {
                    for parent in commit.parents {
                        if seen.insert(parent) && !alternates.contains(&parent) {
                            queue.push_back(parent);
                        }
                    }
                }
                if seen.insert(commit.tree) && !alternates.contains(&commit.tree) {
                    queue.push_back(commit.tree);
                }
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    // Skip submodule (gitlink) entries.
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if !seen.insert(entry.oid) {
                        continue;
                    }
                    if alternates.contains(&entry.oid) {
                        continue;
                    }
                    // Classify by file type bits, as Git does, so non-canonical directory
                    // modes from old trees still count as trees.
                    if entry.mode & S_IFMT == S_IFDIR {
                        queue.push_back(entry.oid);
                    } else {
                        blobs.push(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                if seen.insert(tag.object) && !alternates.contains(&tag.object) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }

    require_objects_exist(odb, &blobs, promised)?;
    Ok(seen)
}

/// Fail with [`Error::ObjectNotFound`] for the first of `oids` that does not exist.
///
/// Local pack indexes are loaded once and searched directly: going through
/// [`Odb::exists`] or [`Odb::read_info`] per object revalidates the pack directory
/// or probes for a loose file each time, which dominates prune on large trees.
fn require_objects_exist(odb: &Odb, oids: &[ObjectId], promised: &HashSet<ObjectId>) -> Result<()> {
    let indexes = odb.local_pack_indexes().unwrap_or_default();
    for oid in oids {
        if promised.contains(oid) {
            continue;
        }
        let packed = indexes.iter().any(|idx| idx.find_position(oid).is_some());
        if !packed && !odb.contains(oid)? {
            return Err(Error::ObjectNotFound(oid.to_hex()));
        }
    }
    Ok(())
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
        Some(&remote_odb),
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
/// Returns [`crate::refs::store::RefStoreError`] for duplicate ref names, CAS failures,
/// lock conflicts, and namespace checks. Propagates other ref I/O errors.
pub fn update_refs(git_dir: &Path, updates: &[RefTransactionItem]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let store_updates: Vec<RefUpdate> = updates
        .iter()
        .map(ref_transaction_item_to_store_update)
        .collect();
    refs::commit_ref_store_batch(git_dir, &store_updates)
}

fn ref_transaction_item_to_store_update(item: &RefTransactionItem) -> RefUpdate {
    let mut flags = RefUpdateFlags::default();
    if item.name == "HEAD" {
        flags.no_deref = true;
    }
    RefUpdate {
        name: item.name.clone(),
        new_value: item.new_oid.map(RawRef::Direct),
        expected: match item.expected_old {
            Some(oid) => Expected::Oid(oid),
            None => Expected::Any,
        },
        reflog: None,
        flags,
    }
}

#[cfg(test)]
mod update_refs_tests {
    use super::*;
    use crate::error::Error;
    use crate::refs::store::RefStoreError;
    use crate::repo::init_repository;
    use tempfile::TempDir;

    fn sample_oid(hex: &str) -> ObjectId {
        hex.parse().expect("oid")
    }

    #[test]
    fn rejects_duplicate_ref_names_in_batch() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
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
        let err = update_refs(&repo.git_dir(), &items).expect_err("duplicate");
        match &err {
            Error::RefStore(RefStoreError::DuplicateUpdate { name }) => {
                assert_eq!(name, "refs/heads/dup");
            }
            other => panic!("expected DuplicateUpdate, got {other}"),
        }
    }

    #[test]
    fn update_refs_typed_errors() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let git_dir = repo.git_dir();
        let c1 = refs::test_commit(&git_dir, "c1");
        let c2 = refs::test_commit(&git_dir, "c2");
        refs::write_ref(&git_dir, "refs/heads/cas", &c2).expect("seed");

        let err = update_refs(
            &git_dir,
            &[RefTransactionItem {
                name: "refs/heads/cas".to_owned(),
                new_oid: Some(c1),
                expected_old: Some(c1),
            }],
        )
        .expect_err("stale cas");
        match &err {
            Error::RefStore(RefStoreError::ExpectedMismatch { name, .. }) => {
                assert_eq!(name, "refs/heads/cas");
            }
            other => panic!("expected ExpectedMismatch, got {other}"),
        }
    }

    #[test]
    fn reftable_cas_rechecked_under_stack_lock() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Reftable,
        )
        .expect("init");
        let git_dir = repo.git_dir();
        let c1 = refs::test_commit(&git_dir, "c1");
        let c2 = refs::test_commit(&git_dir, "c2");
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
        assert!(
            matches!(err, Error::RefStore(RefStoreError::ExpectedMismatch { .. })),
            "{err}"
        );
        assert_eq!(
            crate::refs::resolve_ref(&git_dir, "refs/heads/locked-cas").unwrap(),
            c2
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn apply_cas_rejects_after_concurrent_packed_delete() {
        let _pause_serial = refs::test_pause_injection_serial_guard();
        use std::path::Path;
        use std::process::Command;
        use std::sync::Arc;
        use std::thread;

        let _pause_scope = refs::ref_store_batch_pause_test_scope();

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn rev_parse(repo: &Path, refname: &str) -> ObjectId {
            let hex = Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", refname])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("rev-parse")
                .stdout;
            String::from_utf8(hex)
                .expect("utf8")
                .trim()
                .parse()
                .expect("oid")
        }

        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main", "."]);
        let git_dir = repo.join(".git");

        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git(&repo, &["add", "f.txt"]);
        git(&repo, &["commit", "-q", "-m", "c1"]);
        let c1 = rev_parse(&repo, "HEAD");

        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        git(&repo, &["add", "f.txt"]);
        git(&repo, &["commit", "-q", "-m", "c2"]);
        let c2 = rev_parse(&repo, "HEAD");

        for name in ["refs/heads/first", "refs/heads/middle", "refs/heads/last"] {
            git(&repo, &["update-ref", name, &c1.to_hex()]);
        }
        git(&repo, &["pack-refs", "--all"]);
        for name in ["first", "middle", "last"] {
            let _ = std::fs::remove_file(git_dir.join(format!("refs/heads/{name}")));
        }

        let gate = refs::new_ref_store_batch_pause_gate();
        refs::set_test_inject_ref_store_batch_pause_after_apply(
            0,
            &git_dir,
            Some(Arc::clone(&gate)),
        );

        let updates = vec![
            RefTransactionItem {
                name: "refs/heads/first".to_owned(),
                new_oid: Some(c2),
                expected_old: Some(c1),
            },
            RefTransactionItem {
                name: "refs/heads/middle".to_owned(),
                new_oid: Some(c2),
                expected_old: Some(c1),
            },
            RefTransactionItem {
                name: "refs/heads/last".to_owned(),
                new_oid: Some(c2),
                expected_old: Some(c1),
            },
        ];

        let git_dir_thread = git_dir.clone();
        let updates_thread = updates.clone();
        let worker = thread::spawn(move || update_refs(&git_dir_thread, &updates_thread));

        let mut paused = gate.wait.lock().expect("lock");
        while !*paused {
            paused = gate.cv.wait(paused).expect("wait");
        }

        git(
            &repo,
            &["update-ref", "-d", "refs/heads/last", &c1.to_hex()],
        );

        *paused = false;
        gate.cv.notify_all();
        drop(paused);

        let err = worker.join().expect("join").expect_err("stale CAS");
        match &err {
            Error::RefStore(RefStoreError::ExpectedMismatch { name, .. }) => {
                assert_eq!(name, "refs/heads/last");
            }
            other => panic!("expected ExpectedMismatch for last ref, got {other}"),
        }
        assert!(crate::refs::resolve_ref(&git_dir, "refs/heads/last").is_err());
        assert_eq!(
            crate::refs::resolve_ref(&git_dir, "refs/heads/first").expect("first"),
            c2
        );

        refs::set_test_inject_ref_store_batch_pause_after_apply(0, &git_dir, None);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn unconditional_create_rejects_after_concurrent_packed_parent() {
        let _pause_serial = refs::test_pause_injection_serial_guard();
        use std::path::Path;
        use std::process::Command;
        use std::sync::Arc;
        use std::thread;

        let _pause_scope = refs::ref_store_batch_pause_test_scope();

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn rev_parse(repo: &Path, refname: &str) -> ObjectId {
            let hex = Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", refname])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("rev-parse")
                .stdout;
            String::from_utf8(hex)
                .expect("utf8")
                .trim()
                .parse()
                .expect("oid")
        }

        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main", "."]);
        let git_dir = repo.join(".git");

        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git(&repo, &["add", "f.txt"]);
        git(&repo, &["commit", "-q", "-m", "c1"]);
        let c1 = rev_parse(&repo, "HEAD");

        let gate = refs::new_ref_store_batch_pause_gate();
        refs::set_test_inject_ref_store_batch_pause_after_apply(
            0,
            &git_dir,
            Some(Arc::clone(&gate)),
        );

        let updates = vec![
            RefTransactionItem {
                name: "refs/heads/one".to_owned(),
                new_oid: Some(c1),
                expected_old: None,
            },
            RefTransactionItem {
                name: "refs/heads/two".to_owned(),
                new_oid: Some(c1),
                expected_old: None,
            },
            RefTransactionItem {
                name: "refs/heads/conflict/child".to_owned(),
                new_oid: Some(c1),
                expected_old: None,
            },
        ];

        let git_dir_thread = git_dir.clone();
        let updates_thread = updates.clone();
        let worker = thread::spawn(move || update_refs(&git_dir_thread, &updates_thread));

        let mut paused = gate.wait.lock().expect("lock");
        while !*paused {
            paused = gate.cv.wait(paused).expect("wait");
        }

        git(&repo, &["update-ref", "refs/heads/conflict", &c1.to_hex()]);
        git(&repo, &["pack-refs", "--all"]);

        *paused = false;
        gate.cv.notify_all();
        drop(paused);

        let err = worker
            .join()
            .expect("join")
            .expect_err("namespace conflict on child");
        assert!(
            err.to_string().contains("namespace conflict")
                || err.to_string().contains("cannot lock ref"),
            "{err}"
        );
        assert!(crate::refs::resolve_ref(&git_dir, "refs/heads/conflict").is_ok());
        assert!(crate::refs::resolve_ref(&git_dir, "refs/heads/conflict/child").is_err());

        refs::set_test_inject_ref_store_batch_pause_after_apply(0, &git_dir, None);
    }
}

#[cfg(test)]
mod prune_walk_tests {
    use super::*;
    use crate::maintenance::{repack, RepackOptions};
    use crate::repo::init_repository;
    use tempfile::TempDir;

    #[test]
    fn promise_boundary_is_shared_and_missing_local_objects_still_fail() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            true,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").expect("oid");
        let mut tree = b"100644 promised\0".to_vec();
        tree.extend_from_slice(missing.as_bytes());
        let tree = repo.odb().write(ObjectKind::Tree, &tree).expect("tree");
        crate::promisor::write_promisor_marker(repo.git_dir(), &HashSet::from([missing]))
            .expect("marker");
        let roots = [tree];
        let shallow = HashSet::new();
        let reachable = LazyReachable::new(repo.odb(), &roots, &shallow);
        assert!(reachable.get().expect("closure").contains(&missing));
        crate::promisor::write_promisor_marker(repo.git_dir(), &HashSet::new())
            .expect("clear marker");
        assert!(reachable
            .promised()
            .expect("cached promises")
            .contains(&missing));
        let next = LazyReachable::new(repo.odb(), &roots, &shallow);
        assert!(matches!(next.get(), Err(Error::ObjectNotFound(_))));
    }

    #[test]
    fn prune_checks_packed_blobs_without_per_object_lookups() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let odb = repo.odb();
        let mut tree = Vec::new();
        for i in 0..50 {
            let blob = odb
                .write(ObjectKind::Blob, format!("blob {i}\n").as_bytes())
                .expect("blob");
            tree.extend_from_slice(format!("100644 f{i:02}\0").as_bytes());
            tree.extend_from_slice(blob.as_bytes());
        }
        let tree = odb.write(ObjectKind::Tree, &tree).expect("tree");
        let commit = format!(
            "tree {tree}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nm\n"
        );
        let commit = odb
            .write(ObjectKind::Commit, commit.as_bytes())
            .expect("commit");
        std::fs::write(
            repo.git_dir().join("refs/heads/main"),
            format!("{}\n", commit.to_hex()),
        )
        .expect("ref");
        repack(&repo, &RepackOptions::default()).expect("repack");
        assert!(odb
            .enumerate_local_loose_objects()
            .expect("loose")
            .is_empty());

        odb.reset_exists_probe();
        let stats = prune_loose_unreachable(odb, &[commit], None).expect("prune");
        assert_eq!(stats, PruneStats::default());
        assert_eq!(
            odb.exists_probe_count(),
            0,
            "packed blobs must be checked against the pack indexes, not one lookup each"
        );
    }
}

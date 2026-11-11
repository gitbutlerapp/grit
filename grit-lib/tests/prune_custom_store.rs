//! Garbage collection and pruning on a custom primary store through
//! [`PrunableObjectStore`], cross-checked against the files backend and the system `git`.

use grit_lib::objects::FileMode;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::maintenance::{GcOptions, PruneOptions};
use grit_lib::objects::odb::store::{
    MemoryStore, ObjectStore, PrunableObjectStore, WritableObjectStore,
};
use grit_lib::objects::odb::WriteOptions;
use grit_lib::objects::{
    serialize_tag, serialize_tree, HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind, TagData,
    TreeEntry,
};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::gc::prune_loose_unreachable;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefName, RefStore, RefTransaction, RefUpdate, ReflogUpdate,
};
use grit_lib::repo::error::FilesOnlyOperation;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use time::OffsetDateTime;

pub(crate) const PRUNE_NOW: &str = "[gc]\n\tpruneExpire = now\n";
const PRECIOUS: &str =
    "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tpreciousObjects = true\n";

pub(crate) fn fixed_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time")
}

/// Initialize a repository in `dir` and append `config` to its `.git/config`.
pub(crate) fn init(dir: &Path, config: &str) {
    Repository::init(
        dir,
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let path = dir.join(".git").join("config");
    let mut text = std::fs::read_to_string(&path).expect("read config");
    text.push_str(config);
    std::fs::write(&path, text).expect("write config");
}

pub(crate) fn open_files(dir: &Path) -> Repository {
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open files repo")
}

fn open_custom(
    dir: &Path,
    store: Arc<dyn WritableObjectStore>,
    refs: Option<Arc<dyn RefStore>>,
) -> Repository {
    let git_dir = dir.join(".git");
    let mut opts = RepositoryOptions::empty()
        .with_object_store(store)
        .with_work_tree(dir);
    if let Some(refs) = refs {
        opts = opts.with_ref_store(refs);
    }
    Repository::open(&git_dir, &opts).expect("open custom repo")
}

pub(crate) fn memory_repo(config: &str) -> (tempfile::TempDir, Repository, Arc<MemoryStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), config);
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = open_custom(dir.path(), store.clone(), None);
    (dir, repo, store)
}

pub(crate) fn stored(store: &dyn ObjectStore) -> BTreeSet<ObjectId> {
    let mut out = BTreeSet::new();
    store
        .for_each_object(&mut |oid| {
            out.insert(*oid);
            ControlFlow::Continue(())
        })
        .expect("list objects");
    out
}

fn blob(repo: &Repository, data: &str) -> ObjectId {
    repo.odb()
        .write(ObjectKind::Blob, data.as_bytes())
        .expect("blob")
}

fn tree(repo: &Repository, entries: &[(&str, ObjectId)]) -> ObjectId {
    let entries: Vec<TreeEntry> = entries
        .iter()
        .map(|(name, oid)| TreeEntry::new(name.as_bytes().to_vec(), FileMode::REGULAR, *oid))
        .collect();
    repo.odb()
        .write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("tree")
}

/// A commit written as raw bytes, so both backends store the same id.
fn commit(repo: &Repository, tree: ObjectId, parent: Option<ObjectId>, message: &str) -> ObjectId {
    let mut body = format!("tree {}\n", tree.to_hex());
    if let Some(parent) = parent {
        body.push_str(&format!("parent {}\n", parent.to_hex()));
    }
    body.push_str("author T <t@example.com> 1700000000 +0000\n");
    body.push_str("committer T <t@example.com> 1700000000 +0000\n\n");
    body.push_str(message);
    body.push('\n');
    repo.odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

fn set_ref(repo: &Repository, name: &str, new: ObjectId, expected: Expected, message: &str) {
    let txn = RefTransaction::new()
        .update(
            RefUpdate::new(
                RefName::new(name).expect("valid ref name"),
                Some(RawRef::Direct(new)),
            )
            .with_expected(expected)
            .with_reflog(Some(ReflogUpdate {
                identity: "T <t@example.com>".into(),
                message: message.into(),
                time: fixed_now(),
                old_oid: None,
                new_oid: None,
            })),
        )
        .expect("transaction");
    repo.ref_store()
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit ref");
}

/// What [`build_history`] wrote.
pub(crate) struct History {
    /// Reachable from a ref tip.
    pub(crate) tip: ObjectId,
    /// Reachable only from the reflog of `refs/heads/side`.
    pub(crate) reflog_only: ObjectId,
    /// Blob that only the index names.
    pub(crate) staged: ObjectId,
    /// Objects nothing reaches.
    pub(crate) unreachable: Vec<ObjectId>,
}

/// The same history in any repository: two commits on `main`, a commit only the `side`
/// reflog reaches, an annotated tag, a staged blob, and unreachable objects.
pub(crate) fn build_history(repo: &Repository, work_tree: &Path) -> History {
    let a = blob(repo, "a\n");
    let c1 = commit(repo, tree(repo, &[("a", a)]), None, "one");
    let b = blob(repo, "b\n");
    let tip = commit(repo, tree(repo, &[("a", a), ("b", b)]), Some(c1), "two");
    set_ref(
        repo,
        "refs/heads/main",
        c1,
        Expected::Missing,
        "branch: Created",
    );
    set_ref(
        repo,
        "refs/heads/main",
        tip,
        Expected::Oid(c1),
        "commit: two",
    );

    let r = blob(repo, "only in a reflog\n");
    let reflog_only = commit(repo, tree(repo, &[("r", r)]), None, "side");
    set_ref(
        repo,
        "refs/heads/side",
        reflog_only,
        Expected::Missing,
        "branch: Created",
    );
    set_ref(
        repo,
        "refs/heads/side",
        tip,
        Expected::Oid(reflog_only),
        "reset: moving",
    );

    let tag = serialize_tag(&TagData::new(
        c1,
        "commit",
        "v1",
        Some("T <t@example.com> 1700000000 +0000".into()),
        "v1\n",
    ));
    let tag = repo.odb().write(ObjectKind::Tag, &tag).expect("tag");
    set_ref(repo, "refs/tags/v1", tag, Expected::Missing, "tag");

    std::fs::write(work_tree.join("staged.txt"), b"staged only\n").expect("write file");
    repo.index()
        .stage_for_commit(&mut NullProgress, &CancelToken::never())
        .expect("stage");
    let staged = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index")
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"staged.txt"))
        .expect("staged entry")
        .oid;

    let orphan = blob(repo, "orphan\n");
    let orphan_tree = tree(repo, &[("orphan", orphan)]);
    let orphan_commit = commit(repo, orphan_tree, Some(tip), "dropped");
    History {
        tip,
        reflog_only,
        staged,
        unreachable: vec![orphan, orphan_tree, orphan_commit],
    }
}

#[track_caller]
fn assert_unsupported<T: std::fmt::Debug>(result: grit_lib::error::Result<T>) {
    match result {
        Err(Error::Repository(RepositoryError::UnsupportedObjectStore { operation })) => {
            assert_eq!(operation, FilesOnlyOperation::Prune);
        }
        other => panic!("expected UnsupportedObjectStore(Prune), got {other:?}"),
    }
}

#[track_caller]
fn assert_precious<T: std::fmt::Debug>(result: grit_lib::error::Result<T>, expected: &str) {
    match result {
        Err(Error::Repository(RepositoryError::PreciousObjectsRepository { operation })) => {
            assert_eq!(operation, expected);
        }
        other => panic!("expected PreciousObjectsRepository({expected}), got {other:?}"),
    }
}

#[test]
fn unreachable_blob_is_removed_and_history_kept() {
    let (dir, repo, store) = memory_repo(PRUNE_NOW);
    let history = build_history(&repo, dir.path());
    let before = stored(store.as_ref());

    let stats = repo
        .maintenance()
        .prune_loose_objects(&PruneOptions::default().with_now(fixed_now()))
        .expect("prune");

    let after = stored(store.as_ref());
    assert_eq!(stats.pruned, history.unreachable.len());
    assert_eq!(stats.kept, after.len());
    for oid in &history.unreachable {
        assert!(!after.contains(oid), "{oid} is unreachable");
    }
    assert_eq!(
        before.difference(&after).count(),
        history.unreachable.len(),
        "only the unreachable objects go"
    );
    assert!(after.contains(&history.tip));
    assert!(after.contains(&history.staged), "the index is a root");
}

#[test]
fn reflog_reachable_commit_and_its_tree_are_kept() {
    let (dir, repo, store) = memory_repo(PRUNE_NOW);
    let history = build_history(&repo, dir.path());
    repo.maintenance()
        .prune_loose_objects(&PruneOptions::default())
        .expect("prune");
    let reflog_commit = repo
        .odb()
        .read(&history.reflog_only)
        .expect("reflog-only commit survives");
    let tree_hex = std::str::from_utf8(&reflog_commit.data)
        .expect("utf-8")
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("tree "))
        .expect("tree line")
        .to_owned();
    let tree_oid = ObjectId::from_hex(&tree_hex).expect("tree oid");
    assert!(store.contains(&tree_oid).expect("contains"));
}

#[test]
fn dry_run_counts_without_removing() {
    let (dir, repo, store) = memory_repo(PRUNE_NOW);
    let history = build_history(&repo, dir.path());
    let before = stored(store.as_ref());
    let mut opts = PruneOptions::default();
    opts.dry_run = true;
    let stats = repo
        .maintenance()
        .prune_loose_objects(&opts)
        .expect("dry run");
    assert_eq!(stats.pruned, history.unreachable.len());
    assert_eq!(stored(store.as_ref()), before);
}

#[test]
fn default_grace_period_keeps_objects_without_a_recorded_time() {
    let (dir, repo, store) = memory_repo("");
    let history = build_history(&repo, dir.path());
    let before = stored(store.as_ref());
    let stats = repo
        .maintenance()
        .prune_loose_objects(&PruneOptions::default().with_now(fixed_now()))
        .expect("prune");
    assert_eq!(stats.pruned, 0);
    assert_eq!(stats.kept, before.len());
    assert!(store.contains(&history.unreachable[0]).expect("contains"));
}

#[test]
fn pruned_objects_are_not_served_from_the_session_cache() {
    let (dir, repo, _store) = memory_repo(PRUNE_NOW);
    let history = build_history(&repo, dir.path());
    let orphan = history.unreachable[0];
    repo.odb().read(&orphan).expect("readable before prune");
    repo.maintenance()
        .prune_loose_objects(&PruneOptions::default())
        .expect("prune");
    assert!(
        repo.odb().read(&orphan).is_err(),
        "pruned blob must be gone"
    );
}

/// A store that records write times in a map the test controls.
#[derive(Debug)]
struct TimedStore {
    inner: MemoryStore,
    times: Mutex<HashMap<ObjectId, SystemTime>>,
}

impl ObjectStore for TimedStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<Object>> {
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for TimedStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> grit_lib::error::Result<ObjectId> {
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.inner.freshen(oid)
    }

    fn as_prunable(&self) -> Option<&dyn PrunableObjectStore> {
        Some(self)
    }
}

impl PrunableObjectStore for TimedStore {
    fn remove(&self, oids: &[ObjectId]) -> grit_lib::error::Result<usize> {
        self.inner.remove(oids)
    }

    fn object_mtime(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<SystemTime>> {
        Ok(self.times.lock().expect("lock").get(oid).copied())
    }
}

#[test]
fn grace_period_uses_object_mtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), "");
    let store = Arc::new(TimedStore {
        inner: MemoryStore::new(HashAlgo::Sha1),
        times: Mutex::new(HashMap::new()),
    });
    let repo = open_custom(dir.path(), store.clone(), None);
    let history = build_history(&repo, dir.path());
    let cutoff = SystemTime::from(fixed_now());
    let old = blob(&repo, "old orphan\n");
    let new = blob(&repo, "new orphan\n");
    let untimed = history.unreachable[0];
    {
        let mut times = store.times.lock().expect("lock");
        times.insert(old, cutoff - Duration::from_secs(60));
        times.insert(new, cutoff + Duration::from_secs(60));
    }

    let stats = repo
        .maintenance()
        .prune_loose_objects(&PruneOptions::with_older_than(Some(cutoff)))
        .expect("prune");

    assert_eq!(stats.pruned, 1);
    assert!(
        !store.contains(&old).expect("contains"),
        "older than the cutoff"
    );
    assert!(
        store.contains(&new).expect("contains"),
        "inside the grace period"
    );
    assert!(store.contains(&untimed).expect("contains"), "age unknown");
}

#[test]
fn precious_objects_refuses_and_gc_deletes_nothing() {
    let (dir, repo, store) = memory_repo(&format!("{PRUNE_NOW}{PRECIOUS}"));
    let history = build_history(&repo, dir.path());
    let before = stored(store.as_ref());

    assert_precious(
        repo.maintenance()
            .prune_loose_objects(&PruneOptions::default()),
        "prune",
    );
    assert_precious(repo.odb().gc(), "gc");
    let mut opts = GcOptions::default();
    opts.now_unix = fixed_now().unix_timestamp();
    let stats = repo
        .maintenance()
        .gc(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("gc runs its other phases");
    assert!(!stats.prune_ran);
    assert_eq!(stored(store.as_ref()), before);
    assert!(store.contains(&history.unreachable[0]).expect("contains"));
}

#[test]
fn precious_objects_refuses_prune_on_files_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), &format!("{PRUNE_NOW}{PRECIOUS}"));
    let repo = open_files(dir.path());
    let orphan = blob(&repo, "orphan\n");
    assert_precious(
        repo.maintenance()
            .prune_loose_objects(&PruneOptions::default()),
        "prune",
    );
    assert_precious(repo.odb().gc(), "gc");
    assert!(repo.odb().object_path(&orphan).is_file());
}

/// `extensions.preciousObjects` set only through the repository's `Environment`
/// (`GIT_CONFIG_COUNT`), not in any config file.
fn precious_environment(dir: &Path) -> Environment {
    Environment::from_vars(
        [
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "extensions.preciousObjects"),
            ("GIT_CONFIG_VALUE_0", "true"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        dir.to_path_buf(),
    )
}

#[test]
fn odb_gc_honors_precious_objects_from_the_injected_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), "[core]\n\trepositoryformatversion = 1\n");
    let git_dir = dir.path().join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::with_environment(precious_environment(dir.path()))
            .with_object_store(store.clone())
            .with_work_tree(dir.path()),
    )
    .expect("open");
    let orphan = blob(&repo, "orphan\n");

    assert_precious(
        repo.maintenance()
            .prune_loose_objects(&PruneOptions::default()),
        "prune",
    );
    assert_precious(repo.odb().gc(), "gc");
    assert!(store.contains(&orphan).expect("contains"));
}

#[test]
fn files_odb_gc_honors_precious_objects_from_the_injected_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), "[core]\n\trepositoryformatversion = 1\n");
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::with_environment(precious_environment(dir.path()))
            .with_work_tree(dir.path()),
    )
    .expect("open");
    let orphan = blob(&repo, "orphan\n");
    assert_precious(repo.odb().gc(), "gc");
    assert!(repo.odb().object_path(&orphan).is_file());
}

/// A writable store that cannot delete objects.
#[derive(Debug)]
struct UnprunableStore(MemoryStore);

impl ObjectStore for UnprunableStore {
    fn hash_algo(&self) -> HashAlgo {
        self.0.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<Object>> {
        self.0.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectInfo>> {
        self.0.read_info(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.0.for_each_object(f)
    }
}

impl WritableObjectStore for UnprunableStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> grit_lib::error::Result<ObjectId> {
        self.0.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.0.freshen(oid)
    }
}

#[test]
fn store_without_capability_gets_typed_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), PRUNE_NOW);
    let store = Arc::new(UnprunableStore(MemoryStore::new(HashAlgo::Sha1)));
    let repo = open_custom(dir.path(), store.clone(), None);
    let history = build_history(&repo, dir.path());
    let before = stored(store.as_ref());

    assert_unsupported(
        repo.maintenance()
            .prune_loose_objects(&PruneOptions::default()),
    );
    assert_unsupported(repo.maintenance().gc(
        &GcOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    ));
    assert_unsupported(repo.odb().gc());
    assert_unsupported(prune_loose_unreachable(repo.odb(), &[history.tip], None));
    assert_eq!(stored(store.as_ref()), before, "nothing was touched");
    assert!(
        !repo.git_dir().join("gc.pid").exists(),
        "the refusal comes before the gc lock"
    );
}

#[test]
fn maintenance_gc_prunes_a_memory_primary() {
    let (dir, repo, store) = memory_repo(PRUNE_NOW);
    let history = build_history(&repo, dir.path());

    let dry = repo
        .maintenance()
        .gc(
            &GcOptions::with_repack_dry_run(true, fixed_now().unix_timestamp()),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("dry run");
    assert_eq!(dry.prune.pruned, history.unreachable.len());
    assert!(!dry.prune_ran);
    assert!(store.contains(&history.unreachable[0]).expect("contains"));

    let mut opts = GcOptions::default();
    opts.now_unix = fixed_now().unix_timestamp();
    let stats = repo
        .maintenance()
        .gc(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("gc");
    assert!(stats.prune_ran);
    assert_eq!(stats.prune.pruned, history.unreachable.len());
    assert!(stats.commit_graph.is_none(), "no objects/info to write to");
    assert_eq!(
        stats.repack,
        Default::default(),
        "repack needs the files layout"
    );
    for oid in &history.unreachable {
        assert!(!store.contains(oid).expect("contains"));
    }
    assert!(store.contains(&history.reflog_only).expect("contains"));
    assert!(store.contains(&history.staged).expect("contains"));
}

/// `Odb::gc` reopens the repository to find its roots; it must use the injected ref
/// store, not the (empty) refs on disk.
#[test]
fn odb_gc_finds_roots_in_an_injected_ref_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path(), "");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let refs: Arc<dyn RefStore> = Arc::new(MemoryRefStore::new());
    let repo = open_custom(dir.path(), store.clone(), Some(refs));
    let a = blob(&repo, "kept\n");
    let tip = commit(&repo, tree(&repo, &[("a", a)]), None, "kept");
    set_ref(
        &repo,
        "refs/heads/main",
        tip,
        Expected::Missing,
        "branch: Created",
    );
    let orphan = blob(&repo, "orphan\n");

    repo.odb().gc().expect("gc");

    assert!(store.contains(&tip).expect("contains"));
    assert!(store.contains(&a).expect("contains"));
    assert!(!store.contains(&orphan).expect("contains"));
}

/// The loose objects under a files repository's `objects/` directory.
fn loose_objects(git_dir: &Path) -> BTreeSet<ObjectId> {
    let mut out = BTreeSet::new();
    for fanout in std::fs::read_dir(git_dir.join("objects")).expect("objects dir") {
        let fanout = fanout.expect("entry");
        let prefix = fanout.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2 || !fanout.path().is_dir() {
            continue;
        }
        for file in std::fs::read_dir(fanout.path()).expect("fan-out dir") {
            let rest = file
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            out.insert(ObjectId::from_hex(&format!("{prefix}{rest}")).expect("oid"));
        }
    }
    out
}

/// The same history pruned on the files backend and on a memory primary leaves the same
/// objects with the same counts.
#[test]
fn files_and_memory_keep_the_same_objects() {
    let files_dir = tempfile::tempdir().expect("tempdir");
    init(files_dir.path(), PRUNE_NOW);
    let files = open_files(files_dir.path());
    let files_history = build_history(&files, files_dir.path());

    let (memory_dir, memory, store) = memory_repo(PRUNE_NOW);
    let memory_history = build_history(&memory, memory_dir.path());
    assert_eq!(files_history.unreachable, memory_history.unreachable);
    assert_eq!(
        loose_objects(files.git_dir()),
        stored(store.as_ref()),
        "same history, same objects"
    );

    let opts = PruneOptions::default().with_now(fixed_now());
    let files_stats = files
        .maintenance()
        .prune_loose_objects(&opts)
        .expect("files prune");
    let memory_stats = memory
        .maintenance()
        .prune_loose_objects(&opts)
        .expect("memory prune");
    assert_eq!(files_stats, memory_stats);
    assert_eq!(files_stats.pruned, files_history.unreachable.len());
    assert_eq!(loose_objects(files.git_dir()), stored(store.as_ref()));
}

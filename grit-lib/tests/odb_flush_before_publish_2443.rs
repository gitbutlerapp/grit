//! Issue #2443: object writes must flush before refs or the index publish them.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::notes::NotesRef;
use grit_lib::objects::index_pack::{ingest_received_pack, IngestPackOptions};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, ObjectStream, WritableObjectStore};
use grit_lib::objects::odb::{OdbBuilder, WriteOptions};
use grit_lib::objects::pack_objects::{build_pack, PackBuildOptions};
use grit_lib::objects::unpack_objects::{ingest_pack_into, UnpackOptions};
use grit_lib::objects::CommitData;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::commit::{create_commit, write_commit_object, CommitRequest};
use grit_lib::porcelain::tag::{create_tag, CreateTagRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::repo_handles::NotesWriteOptions;
use grit_lib::revwalk::history::{
    apply_rewrite, reword, ApplyOptions, EmptyPolicy, RefScope, RewriteOptions,
};
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::PostOperationHooks;

#[derive(Clone, Default)]
struct StoreState {
    staged: HashSet<ObjectId>,
    committed: HashSet<ObjectId>,
    events: Vec<&'static str>,
}

struct TransactionalStore {
    inner: MemoryStore,
    state: Arc<Mutex<StoreState>>,
    fail_flush: Arc<AtomicBool>,
}

impl fmt::Debug for TransactionalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransactionalStore").finish_non_exhaustive()
    }
}

impl TransactionalStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            state: Arc::new(Mutex::new(StoreState::default())),
            fail_flush: Arc::new(AtomicBool::new(false)),
        }
    }

    fn events(&self) -> Vec<&'static str> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .clone()
    }

    fn durable(&self, oid: &ObjectId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .committed
            .contains(oid)
    }

    fn simulate_restart_drop_staged(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.staged.clear();
    }

    fn set_fail_flush(&self, fail: bool) {
        self.fail_flush.store(fail, Ordering::Relaxed);
    }
}

impl ObjectStore for TransactionalStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<Object>> {
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.inner.contains(oid)
    }

    fn contains_many(&self, oids: &[ObjectId]) -> grit_lib::error::Result<Vec<bool>> {
        self.inner.contains_many(oids)
    }

    fn open_stream(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectStream<'_>>> {
        self.inner.open_stream(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for TransactionalStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> grit_lib::error::Result<ObjectId> {
        let oid = self.inner.write(kind, data, options)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.staged.insert(oid);
        state.events.push("obj.write");
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.inner.freshen(oid)
    }

    fn flush(&self) -> grit_lib::error::Result<()> {
        if self.fail_flush.load(Ordering::Relaxed) {
            return Err(Error::Io(std::io::Error::other("injected flush failure")));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let drained: Vec<ObjectId> = state.staged.drain().collect();
        state.committed.extend(drained);
        state.events.push("obj.flush");
        Ok(())
    }

    fn rollback(&self) -> grit_lib::error::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.staged.clear();
        state.events.push("obj.rollback");
        Ok(())
    }
}

fn open_repo(gd: &Path, wt: &Path) -> (Repository, Arc<TransactionalStore>) {
    fs::create_dir_all(wt).expect("worktree");
    let store = Arc::new(TransactionalStore::new());
    let primary: Arc<dyn WritableObjectStore> = store.clone();
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(gd.join("objects"))
                .primary(primary)
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(gd, Some(wt))
        .expect("open");
    (repo, store)
}

fn ident() -> grit_lib::ident::Signature {
    "T <t@example.com> 1700000000 +0000"
        .parse()
        .expect("signature")
}

fn seed_commit(repo: &Repository) -> ObjectId {
    fs::write(
        repo.work_tree().expect("worktree").join("file.txt"),
        b"base\n",
    )
    .expect("write");
    stage(
        repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage seed");
    let mut req = CommitRequest::new("base");
    req.author = Some(ident());
    req.committer = Some(ident());
    req.assume_worktree_synced = true;
    create_commit(repo, &req, &mut NullProgress, &NeverCancel)
        .expect("commit")
        .oid
}

fn rewrite_options() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: ident(),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_options() -> ApplyOptions {
    ApplyOptions {
        identity: ident(),
        reflog_message: "history".into(),
        record_operation: None,
        hooks: PostOperationHooks::Skip,
    }
}

#[test]
fn create_tag_flushes_before_ref_update() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gd = tmp.path().join("repo.git");
    let wt = tmp.path().join("wt");
    let (repo, store) = open_repo(&gd, &wt);
    let tip = seed_commit(&repo);
    store.state.lock().unwrap().events.clear();

    create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: tip,
            message: Some("release\n".into()),
            tagger: Some("T <t@example.com> 1700000000 +0000".into()),
            sign: Some(false),
            signing_key: None,
            tree_sha256: None,
            force: false,
            dry_run: false,
        },
    )
    .expect("tag");

    let tag_oid = resolve_revision(&repo, "refs/tags/v1").expect("tag ref");
    assert!(
        store.durable(&tag_oid),
        "tag object must be flushed before ref update"
    );
    assert!(
        store.events().iter().any(|e| *e == "obj.flush"),
        "expected flush: {:?}",
        store.events()
    );
}

#[test]
fn notes_add_flushes_before_ref_update() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gd = tmp.path().join("repo.git");
    let wt = tmp.path().join("wt");
    let (repo, store) = open_repo(&gd, &wt);
    let tip = seed_commit(&repo);
    store.state.lock().unwrap().events.clear();

    let opts = NotesWriteOptions {
        message: "note\n".into(),
        committer: ident(),
        author: None,
    };
    repo.notes()
        .add(&NotesRef::commits(), &tip, b"note\n", &opts, false)
        .expect("add note");

    let notes_tip = resolve_revision(&repo, "refs/notes/commits").expect("notes ref");
    assert!(store.durable(&notes_tip));
    assert!(store.events().iter().any(|e| *e == "obj.flush"));
}

#[test]
fn reword_apply_flushes_before_ref_update() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gd = tmp.path().join("repo.git");
    let wt = tmp.path().join("wt");
    let (repo, store) = open_repo(&gd, &wt);
    let base = seed_commit(&repo);
    store.state.lock().unwrap().events.clear();

    let plan = reword(&repo, base, "reworded\n".into(), &rewrite_options()).expect("plan");
    apply_rewrite(
        &repo,
        &plan,
        &apply_options(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("apply");

    let main = resolve_revision(&repo, "refs/heads/main").expect("main");
    assert_ne!(main, base);
    assert!(
        store.durable(&main),
        "rewritten tip must be durable before ref moved"
    );
    store.simulate_restart_drop_staged();
    let _ = repo
        .odb()
        .read(&main)
        .expect("main tip readable after simulated restart");
}

#[test]
fn stage_flushes_before_index_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gd = tmp.path().join("repo.git");
    let wt = tmp.path().join("wt");
    let (repo, store) = open_repo(&gd, &wt);
    seed_commit(&repo);
    fs::write(repo.work_tree().expect("wt").join("new.txt"), b"new\n").expect("write");
    store.state.lock().unwrap().events.clear();

    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    assert!(
        store.events().iter().any(|e| *e == "obj.flush"),
        "stage must flush before index references new blobs: {:?}",
        store.events()
    );
    assert!(
        !store.state.lock().unwrap().committed.is_empty(),
        "staged blobs must be durable before index write"
    );
}

#[test]
fn write_commit_object_flush_failure_rolls_back_staged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gd = tmp.path().join("repo.git");
    let wt = tmp.path().join("wt");
    let (repo, store) = open_repo(&gd, &wt);
    seed_commit(&repo);

    let blob = repo.odb().write(ObjectKind::Blob, b"x\n").expect("blob");
    store.set_fail_flush(true);
    let data = CommitData {
        tree: repo.odb().empty_tree_oid().expect("empty tree"),
        parents: vec![resolve_revision(&repo, "HEAD").expect("head")],
        author: "T <t@example.com> 1700000000 +0000".into(),
        committer: "T <t@example.com> 1700000000 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "fail flush\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let err = write_commit_object(&repo, &data, Some(false)).expect_err("flush fails");
    assert!(matches!(err, Error::Io(_)));
    assert!(
        store.events().iter().any(|e| *e == "obj.rollback"),
        "rollback after failed flush: {:?}",
        store.events()
    );
    assert!(
        !store.durable(&blob),
        "failed batch must not become durable"
    );

    store.set_fail_flush(false);
    let _ = write_commit_object(&repo, &data, Some(false)).expect("retry");
    assert!(
        !store.durable(&blob),
        "rolled-back blob must not leak into a later flush"
    );
}

#[test]
fn ingest_flush_failure_rolls_back_staged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_gd = tmp.path().join("src.git");
    let src_wt = tmp.path().join("src-wt");
    let (source, _) = open_repo(&src_gd, &src_wt);
    let tip = seed_commit(&source);
    let pack = build_pack(source.odb(), &[tip], &[], &PackBuildOptions::default()).expect("pack");

    let store = Arc::new(TransactionalStore::new());
    store.set_fail_flush(true);
    let bases = MemoryStore::new(HashAlgo::Sha1);
    let err = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &bases,
        &UnpackOptions::default(),
    )
    .expect_err("flush fails");
    assert!(matches!(err, Error::Io(_)));
    assert!(store.events().iter().any(|e| *e == "obj.rollback"));
    assert!(
        store.state.lock().unwrap().committed.is_empty(),
        "failed ingest must not commit staged objects"
    );

    store.set_fail_flush(false);
    let gd = tmp.path().join("dest.git");
    let primary: Arc<dyn WritableObjectStore> = store.clone();
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(gd.join("objects"))
                .primary(primary)
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(&gd, None)
        .expect("open");
    ingest_received_pack(pack, repo.odb(), &IngestPackOptions::default()).expect("retry ingest");
    assert!(!store.state.lock().unwrap().committed.is_empty());
}

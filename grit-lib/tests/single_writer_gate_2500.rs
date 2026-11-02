//! Issue #2500: fair FIFO waiting for single-slot ref stores (not exponential backoff starvation).

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::{
    MemoryRefStore, PreparedRefTransaction, RawRef, RefName, RefStore, RefStoreError,
    RefTransaction,
};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

#[derive(Debug)]
struct Slow {
    inner: MemoryRefStore,
    delay: Duration,
}

struct SlowPrep<'a> {
    inner: Box<dyn PreparedRefTransaction + Send + 'a>,
    delay: Duration,
}

impl PreparedRefTransaction for SlowPrep<'_> {
    fn commit(self: Box<Self>) -> Result<(), RefStoreError> {
        std::thread::sleep(self.delay);
        self.inner.commit()
    }

    fn abort(self: Box<Self>) -> Result<(), RefStoreError> {
        self.inner.abort()
    }
}

impl RefStore for Slow {
    fn format(&self) -> grit_lib::refs::store::RefStorageFormat {
        self.inner.format()
    }

    fn read_raw(&self, name: &str) -> grit_lib::error::Result<Option<RawRef>> {
        self.inner.read_raw(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&grit_lib::refs::store::RefEntry) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        self.inner
            .prepare_for(txn, grit_lib::lockfile::LockWait::Immediately)
    }

    fn prepare_for(
        &self,
        txn: RefTransaction,
        wait: grit_lib::lockfile::LockWait,
    ) -> Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        Ok(Box::new(SlowPrep {
            inner: self.inner.prepare_for(txn, wait)?,
            delay: self.delay,
        }))
    }

    fn reflog_exists(&self, name: &str) -> grit_lib::error::Result<bool> {
        self.inner.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> grit_lib::error::Result<()> {
        self.inner.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> grit_lib::error::Result<()> {
        self.inner.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> grit_lib::error::Result<()> {
        self.inner.replace_reflog(name, entries)
    }

    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> grit_lib::error::Result<Vec<ReflogEntry>>,
    ) -> grit_lib::error::Result<()> {
        self.inner.rewrite_reflog(name, transform)
    }

    fn for_each_reflog_ref(
        &self,
        f: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_reflog_ref(f)
    }
}

struct ConcurrentRun {
    ok: usize,
    fail: usize,
    elapsed: Duration,
    repo: Arc<Repository>,
    tree: ObjectId,
    threads: usize,
    per_thread: usize,
}

fn run_concurrent(threads: usize, per_thread: usize, delay_ms: u64) -> ConcurrentRun {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main"),
    )
    .expect("init");
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(dir.path().join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(Arc::new(Slow {
            inner: MemoryRefStore::new(),
            delay: Duration::from_millis(delay_ms),
        }))
        .initial_branch("main")
        .open(dir.path(), None)
        .expect("open");

    let repo = Arc::new(repo);
    let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
    let ok = AtomicUsize::new(0);
    let fail = AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|s| {
        for th in 0..threads {
            let repo = Arc::clone(&repo);
            let (ok, fail) = (&ok, &fail);
            s.spawn(move || {
                for i in 0..per_thread {
                    let body = format!(
                        "tree {tree}\nauthor A <a@x> {i} +0000\ncommitter A <a@x> {i} +0000\n\nt{th} i{i}\n"
                    );
                    let c = repo
                        .odb()
                        .write(ObjectKind::Commit, body.as_bytes())
                        .expect("commit");
                    let name =
                        RefName::try_from(format!("refs/heads/t{th}").as_str()).expect("name");
                    match repo.references().update(&name, &c) {
                        Ok(()) => {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            fail.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    ConcurrentRun {
        ok: ok.load(Ordering::Relaxed),
        fail: fail.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
        repo,
        tree,
        threads,
        per_thread,
    }
}

fn assert_each_branch_at_final_oid(run: &ConcurrentRun) {
    for th in 0..run.threads {
        let i = run.per_thread - 1;
        let body = format!(
            "tree {}\nauthor A <a@x> {i} +0000\ncommitter A <a@x> {i} +0000\n\nt{th} i{i}\n",
            run.tree
        );
        let expected = run
            .repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("expected oid");
        let name = RefName::try_from(format!("refs/heads/t{th}").as_str()).expect("name");
        let resolved = run.repo.references().resolve(&name).expect("resolve");
        assert_eq!(
            resolved, expected,
            "refs/heads/t{th} should point at the last update"
        );
    }
}

#[test]
fn disjoint_refs_under_slow_single_slot_store_all_succeed() {
    let run = run_concurrent(4, 20, 3);
    assert_eq!(run.fail, 0, "expected zero LockHeld failures");
    assert_eq!(run.ok, 80);
    assert!(
        run.elapsed < Duration::from_secs(2),
        "serialized work should finish quickly, took {:?}",
        run.elapsed
    );
    assert_each_branch_at_final_oid(&run);
}

#[test]
fn disjoint_refs_sixteen_threads_under_slow_single_slot_store() {
    let run = run_concurrent(16, 10, 3);
    assert_eq!(run.fail, 0);
    assert_eq!(run.ok, 160);
    assert_each_branch_at_final_oid(&run);
}

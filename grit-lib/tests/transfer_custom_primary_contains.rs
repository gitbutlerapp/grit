//! Local path / file:// fetch into a custom primary must not call `contains` per object
//! (issue #2446).

use grit_test_support::git;
use std::fs;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, ObjectStream, WritableObjectStore};
use grit_lib::objects::odb::WriteOptions;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::progress::CancelToken;
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::remote::{Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository};

#[derive(Debug)]
struct CountingStore {
    inner: MemoryStore,
    contains: AtomicUsize,
    contains_many: AtomicUsize,
    read_info: AtomicUsize,
    writes: AtomicUsize,
    fail_contains_many: bool,
}

impl CountingStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            contains: AtomicUsize::new(0),
            contains_many: AtomicUsize::new(0),
            read_info: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            fail_contains_many: false,
        })
    }

    fn with_failing_contains_many() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            contains: AtomicUsize::new(0),
            contains_many: AtomicUsize::new(0),
            read_info: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            fail_contains_many: true,
        })
    }

    fn count_objects(&self) -> usize {
        let mut n = 0usize;
        self.inner
            .for_each_object(&mut |_| {
                n += 1;
                ControlFlow::Continue(())
            })
            .expect("enumerate");
        n
    }
}

impl ObjectStore for CountingStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<Object>> {
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectInfo>> {
        self.read_info.fetch_add(1, Ordering::Relaxed);
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.contains.fetch_add(1, Ordering::Relaxed);
        self.inner.contains(oid)
    }

    fn contains_many(&self, oids: &[ObjectId]) -> grit_lib::error::Result<Vec<bool>> {
        self.contains_many.fetch_add(1, Ordering::Relaxed);
        if self.fail_contains_many {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "contains_many probe failed",
            )));
        }
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

impl WritableObjectStore for CountingStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> grit_lib::error::Result<ObjectId> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.inner.freshen(oid)
    }
}

fn git_upstream_no_tags() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    fs::write(dir.path().join("b.txt"), "two\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c2"]);
    dir
}

fn git_upstream() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    git(dir.path(), &["tag", "lightweight"]);
    fs::write(dir.path().join("b.txt"), "two\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c2"]);
    git(
        dir.path(),
        &["tag", "-a", "annotated", "-m", "annotated tag"],
    );
    dir
}

fn memory_repo(store: Arc<CountingStore>) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store)
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_work_tree(dir.path())),
    )
    .expect("open");
    (dir, repo)
}

fn fetch_heads_only() -> FetchOptions {
    let mut opts = FetchOptions::default();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];
    opts
}

fn fetch_all() -> FetchOptions {
    let mut opts = fetch_heads_only();
    opts.refspecs.push("+refs/tags/*:refs/tags/*".into());
    opts
}

fn incremental_second_fetch_read_info(upstream: &tempfile::TempDir, with_tags: bool) -> usize {
    let store = CountingStore::new();
    let (_dir, repo) = memory_repo(store.clone());
    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    let opts = if with_tags {
        fetch_all()
    } else {
        fetch_heads_only()
    };
    repo.remotes()
        .fetch(
            &remote,
            opts.clone(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("first fetch");
    let read_info_after_first = store.read_info.load(Ordering::Relaxed);
    fs::write(upstream.path().join("c.txt"), "three\n").expect("write");
    git(upstream.path(), &["add", "c.txt"]);
    git(upstream.path(), &["commit", "-q", "-m", "c3"]);
    repo.remotes()
        .fetch(&remote, opts, &mut NoProgress, &CancelToken::never(), None)
        .expect("second fetch");
    store.read_info.load(Ordering::Relaxed) - read_info_after_first
}

fn fresh_fetch_read_info(upstream: &tempfile::TempDir, with_tags: bool) -> usize {
    let store = CountingStore::new();
    let (_dir, repo) = memory_repo(store.clone());
    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    let opts = if with_tags {
        fetch_all()
    } else {
        fetch_heads_only()
    };
    repo.remotes()
        .fetch(&remote, opts, &mut NoProgress, &CancelToken::never(), None)
        .expect("fetch");
    store.read_info.load(Ordering::Relaxed)
}

#[test]
fn local_fetch_into_empty_custom_primary_skips_per_object_contains() {
    let upstream = git_upstream();
    let store = CountingStore::new();
    let (_dir, repo) = memory_repo(store.clone());

    let url = format!("file://{}", upstream.path().display());
    let remote = Remote::from_url(&url).expect("remote");
    repo.remotes()
        .fetch(
            &remote,
            fetch_all(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("fetch");

    let objects = store.count_objects();
    assert!(objects >= 5, "fixture should store several objects");
    assert_eq!(
        store.contains.load(Ordering::Relaxed),
        0,
        "local fetch must not call contains per object"
    );
    assert_eq!(
        store.writes.load(Ordering::Relaxed),
        objects,
        "one write per stored object"
    );
    assert_eq!(
        store.contains_many.load(Ordering::Relaxed),
        1,
        "fresh fetch uses one missing_oids contains_many batch (heads and tags)"
    );
}

#[test]
fn incremental_local_fetch_uses_one_contains_many_batch() {
    let upstream = git_upstream();
    let store = CountingStore::new();
    let (_dir, repo) = memory_repo(store.clone());
    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");

    repo.remotes()
        .fetch(
            &remote,
            fetch_all(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("first fetch");
    let writes_after_first = store.writes.load(Ordering::Relaxed);
    let objects_after_first = store.count_objects();
    let contains_many_before_second = store.contains_many.load(Ordering::Relaxed);

    fs::write(upstream.path().join("c.txt"), "three\n").expect("write");
    git(upstream.path(), &["add", "c.txt"]);
    git(upstream.path(), &["commit", "-q", "-m", "c3"]);

    repo.remotes()
        .fetch(
            &remote,
            fetch_all(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("second fetch");

    assert_eq!(
        store.contains.load(Ordering::Relaxed),
        0,
        "incremental fetch must not call contains per object"
    );
    let contains_many_second_delta =
        store.contains_many.load(Ordering::Relaxed) - contains_many_before_second;
    assert_eq!(
        contains_many_second_delta, 3,
        "incremental fetch: missing_oids, send-set missing_materialized_many, and one tag batch"
    );
    let new_writes = store.writes.load(Ordering::Relaxed) - writes_after_first;
    let new_objects = store.count_objects() - objects_after_first;
    assert_eq!(
        new_writes, new_objects,
        "only missing objects are written on incremental fetch"
    );
    assert!(new_writes >= 3, "new commit should add several objects");
    assert!(
        new_writes < objects_after_first,
        "incremental fetch must not rewrite the whole history"
    );
}

#[test]
fn tag_validation_does_not_add_scalar_read_info_beyond_no_tag_fetch() {
    let tagged = git_upstream();
    let plain = git_upstream_no_tags();
    assert_eq!(
        incremental_second_fetch_read_info(&tagged, true),
        incremental_second_fetch_read_info(&plain, false),
        "incremental fetch: tag validation must not add read_info beyond the no-tag path"
    );
}

#[test]
fn fresh_tagged_fetch_does_not_add_read_info_beyond_heads_only_on_same_upstream() {
    let upstream = git_upstream();
    assert_eq!(
        fresh_fetch_read_info(&upstream, true),
        fresh_fetch_read_info(&upstream, false),
        "same upstream: explicit tag refspec must not add read_info beyond heads-only fetch"
    );
}

#[test]
fn local_fetch_propagates_contains_many_errors() {
    let upstream = git_upstream();
    let store = CountingStore::with_failing_contains_many();
    let (_dir, repo) = memory_repo(store);

    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    let err = repo
        .remotes()
        .fetch(
            &remote,
            fetch_all(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect_err("contains_many failure must abort fetch");
    assert!(
        matches!(err, RemoteError::Nested(boxed) if matches!(boxed.as_ref(), Error::Io(_))),
        "expected io error, got {err:?}"
    );
}

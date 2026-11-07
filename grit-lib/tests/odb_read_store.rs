//! [`Odb::read_store`]: one [`ObjectStore`] over everything a repository can read, and the
//! forwarding [`ObjectStore`] impls for `Arc<T>`, `Box<T>` and `&T` that let decorators wrap
//! a shared store.

use std::fs;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::{Error, Result};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::error::FilesOnlyOperation;
use grit_lib::repo::{Repository, RepositoryError};
use grit_test_support::odb_conformance::run_read_suite;
use grit_test_support::{git, git_packed_history, rev_parse};

fn isolated_options() -> RepositoryOptions {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    RepositoryOptions::with_environment(env)
}

/// A repository with no on-disk layout: memory primary, memory refs, the given read sources.
fn memory_repo(git_dir: &Path, read_sources: Vec<Arc<dyn ObjectStore>>) -> Repository {
    layered_repo(
        git_dir,
        Arc::new(MemoryStore::new(HashAlgo::Sha1)),
        read_sources,
    )
}

/// A repository over `primary` and `read_sources` with memory refs and no alternates.
fn layered_repo(
    git_dir: &Path,
    primary: Arc<dyn WritableObjectStore>,
    read_sources: Vec<Arc<dyn ObjectStore>>,
) -> Repository {
    let layers = read_sources
        .into_iter()
        .fold(OdbBuilder::new(), OdbBuilder::push_read_source);
    Repository::open(
        git_dir,
        &(isolated_options()
            .with_object_store(primary)
            .with_odb_builder(layers)
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_initial_branch("main")),
    )
    .expect("open memory repository")
}

fn write_blob(store: &dyn WritableObjectStore, data: &[u8]) -> ObjectId {
    store
        .write(ObjectKind::Blob, data, WriteOptions::default())
        .expect("write blob")
}

/// The issue's reproduction: a custom primary, a pushed read source and an alternate.
/// `read_store` sees all three; `sources` stays the alternates composite and `primary`
/// still refuses a custom store.
#[test]
fn read_store_covers_primary_read_sources_and_alternates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let alt_objects = tmp.path().join("alt/objects");
    let alt_oid = Odb::new(&alt_objects)
        .write(ObjectKind::Blob, b"from alternate")
        .expect("write alternate");
    let git_dir = tmp.path().join("main");
    let objects = git_dir.join("objects");
    fs::create_dir_all(objects.join("info")).expect("info dir");
    fs::write(
        objects.join("info/alternates"),
        format!("{}\n", alt_objects.display()),
    )
    .expect("alternates file");

    let extra = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let extra_oid = write_blob(extra.as_ref(), b"from read source");
    let repo = Repository::open(
        &git_dir,
        &(isolated_options()
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .with_odb_builder(OdbBuilder::new().push_read_source(extra).alternates(true))
            .with_ref_store(Arc::new(MemoryRefStore::new()))),
    )
    .expect("open");
    let odb = repo.odb();
    let primary_oid = odb.write(ObjectKind::Blob, b"from primary").expect("write");

    let store = odb.read_store();
    assert_eq!(store.hash_algo(), HashAlgo::Sha1);
    for (oid, data) in [
        (primary_oid, &b"from primary"[..]),
        (extra_oid, b"from read source"),
        (alt_oid, b"from alternate"),
    ] {
        assert!(store.contains(&oid).expect("contains"), "{oid}");
        let obj = store.read(&oid).expect("read").expect("present");
        assert_eq!((obj.kind, obj.data.as_slice()), (ObjectKind::Blob, data));
        let info = store.read_info(&oid).expect("read_info").expect("present");
        assert_eq!(info.size, data.len() as u64);
        let mut stream = store.open_stream(&oid).expect("stream").expect("present");
        let mut streamed = Vec::new();
        stream
            .reader
            .read_to_end(&mut streamed)
            .expect("read stream");
        assert_eq!(streamed, data);
    }
    assert_eq!(
        store
            .contains_many(&[alt_oid, ObjectId::zero(), extra_oid, primary_oid])
            .expect("contains_many"),
        [true, false, true, true]
    );

    let mut listed = Vec::new();
    store
        .for_each_object(&mut |oid| {
            listed.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("for_each_object");
    listed.sort();
    let mut expected = vec![primary_oid, extra_oid, alt_oid];
    expected.sort();
    assert_eq!(listed, expected);

    let mut matches = Vec::new();
    store
        .lookup_prefix(&extra_oid.to_hex()[..10], 0, &mut matches)
        .expect("lookup_prefix");
    assert_eq!(matches, [extra_oid]);
    store.refresh().expect("refresh");

    let sources = odb.sources().expect("sources");
    assert!(sources.contains(&alt_oid).expect("alternate"));
    assert!(!sources.contains(&primary_oid).expect("primary"));
    assert!(!sources.contains(&extra_oid).expect("read source"));
    assert!(matches!(
        odb.primary(),
        Err(Error::Repository(RepositoryError::UnsupportedObjectStore {
            operation: FilesOnlyOperation::FilesSource,
        }))
    ));
}

/// Exactly the reported program: a repository opened with a memory primary and no alternates.
#[test]
fn read_store_of_custom_primary_repository_sees_written_object() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = memory_repo(&tmp.path().join("g"), Vec::new());
    let oid = repo.odb().write(ObjectKind::Blob, b"x").expect("write");
    assert!(repo.odb().contains(&oid).expect("odb contains"));
    assert!(repo.odb().read_store().contains(&oid).expect("contains"));
    assert!(!repo
        .odb()
        .sources()
        .expect("sources")
        .contains(&oid)
        .expect("sources contains"));
}

#[test]
fn read_store_reports_missing_objects_as_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = memory_repo(&tmp.path().join("g"), Vec::new());
    let store = repo.odb().read_store();
    let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").expect("oid");
    assert!(store.read(&missing).expect("read").is_none());
    assert!(store.read_info(&missing).expect("read_info").is_none());
    assert!(store.open_stream(&missing).expect("open_stream").is_none());
    let mut buf = vec![1, 2, 3];
    assert!(store
        .read_into(&missing, &mut buf)
        .expect("read_into")
        .is_none());
    assert!(!store.contains(&missing).expect("contains"));
}

/// The empty tree is always readable from an `Odb`, so its store view agrees on every path.
#[test]
fn read_store_serves_the_empty_tree_on_every_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = memory_repo(&tmp.path().join("g"), Vec::new());
    let store = repo.odb().read_store();
    let empty = repo.odb().empty_tree_oid().expect("empty tree id");
    assert!(store.contains(&empty).expect("contains"));
    let obj = store.read(&empty).expect("read").expect("empty tree");
    assert_eq!((obj.kind, obj.data.len()), (ObjectKind::Tree, 0));
    let info = store.read_info(&empty).expect("read_info").expect("info");
    assert_eq!((info.kind, info.size), (ObjectKind::Tree, 0));
    let stream = store.open_stream(&empty).expect("stream").expect("stream");
    assert_eq!((stream.kind, stream.size), (ObjectKind::Tree, 0));
}

#[test]
fn read_store_of_custom_primary_passes_read_conformance() {
    run_read_suite(|specs: &[(ObjectKind, Vec<u8>)]| {
        let algo = HashAlgo::Sha1;
        let primary = Arc::new(MemoryStore::new(algo));
        let extra = Arc::new(MemoryStore::new(algo));
        // Odd specs land in the read source, so the suite exercises both layers.
        for (i, (kind, data)) in specs.iter().enumerate() {
            let target: &dyn WritableObjectStore = if i % 2 == 0 {
                primary.as_ref()
            } else {
                extra.as_ref()
            };
            target
                .write(*kind, data, WriteOptions::default())
                .expect("seed");
        }
        layered_repo(Path::new("/nonexistent/read-store"), primary, vec![extra])
            .odb()
            .read_store()
    });
}

#[test]
fn smart_pointers_pass_read_conformance() {
    fn seeded(specs: &[(ObjectKind, Vec<u8>)]) -> MemoryStore {
        let store = MemoryStore::new(HashAlgo::Sha1);
        for (kind, data) in specs {
            store
                .write(*kind, data, WriteOptions::default())
                .expect("seed");
        }
        store
    }
    run_read_suite(|specs: &[(ObjectKind, Vec<u8>)]| Arc::new(seeded(specs)));
    run_read_suite(|specs: &[(ObjectKind, Vec<u8>)]| Box::new(seeded(specs)));
    run_read_suite(|specs: &[(ObjectKind, Vec<u8>)]| -> Arc<dyn ObjectStore> {
        Arc::new(seeded(specs))
    });
}

#[test]
fn smart_pointers_forward_writes() {
    let shared: Arc<dyn WritableObjectStore> = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let oid = WritableObjectStore::write(&shared, ObjectKind::Blob, b"via arc", Default::default())
        .expect("write via Arc");
    assert!(WritableObjectStore::freshen(&shared, &oid).expect("freshen"));
    let boxed: Box<dyn WritableObjectStore> = Box::new(MemoryStore::new(HashAlgo::Sha1));
    let mut payload: &[u8] = b"via box";
    let streamed = boxed
        .write_stream(ObjectKind::Blob, 7, &mut payload, WriteOptions::default())
        .expect("write_stream via Box");
    let borrowed: &MemoryStore = &MemoryStore::new(HashAlgo::Sha1);
    let by_ref =
        WritableObjectStore::write(&borrowed, ObjectKind::Blob, b"via box", Default::default())
            .expect("write via &T");
    assert_eq!(streamed, by_ref);
    assert!(ObjectStore::contains(&borrowed, &by_ref).expect("contains via &T"));
}

/// A decorator generic over any store: the second half of the issue.
#[derive(Debug)]
struct Counted<S> {
    inner: S,
    reads: AtomicUsize,
}

impl<S: ObjectStore> ObjectStore for Counted<S> {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.contains(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.inner.for_each_object(f)
    }
}

#[test]
fn decorator_over_shared_store_layers_one_repository_under_another() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = memory_repo(&tmp.path().join("upstream"), Vec::new());
    let oid = upstream
        .odb()
        .write(ObjectKind::Blob, b"shared")
        .expect("write upstream");

    let shared: Arc<dyn ObjectStore> = upstream.odb().read_store();
    let counted = Arc::new(Counted {
        inner: Arc::clone(&shared),
        reads: AtomicUsize::new(0),
    });
    let mirror = memory_repo(
        &tmp.path().join("mirror"),
        vec![Arc::clone(&counted) as Arc<dyn ObjectStore>],
    );
    assert!(mirror.odb().contains(&oid).expect("contains"));
    assert_eq!(mirror.odb().read(&oid).expect("read").data, b"shared");
    assert!(counted.reads.load(Ordering::Relaxed) >= 2);
    assert!(!counted
        .contains(&ObjectId::zero())
        .expect("decorator contains"));
}

/// Git-written packs read through `read_store`, layered under a memory repository; the
/// combined object set copied into a fresh git repository passes `git fsck --strict`.
#[test]
fn read_store_round_trips_git_packed_history_through_a_memory_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let history = git_packed_history(tmp.path(), 6);
    let tip = rev_parse(&history, "main");
    let tip_tree = rev_parse(&history, "main^{tree}");
    let upstream = Repository::open(&history, &isolated_options()).expect("open history");

    let layered = memory_repo(
        &tmp.path().join("layered"),
        vec![upstream.odb().read_store()],
    );
    let commit = format!(
        "tree {tip_tree}\nparent {tip}\n\
         author A U Thor <author@example.com> 1700000000 +0000\n\
         committer A U Thor <author@example.com> 1700000000 +0000\n\nlayered\n"
    );
    let new_tip = layered
        .odb()
        .write(ObjectKind::Commit, commit.as_bytes())
        .expect("write commit");

    let mut everything = Vec::new();
    layered
        .odb()
        .read_store()
        .for_each_object(&mut |oid| {
            everything.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("for_each_object");
    let git_count: usize = git(&history, &["count-objects", "-v"])
        .lines()
        .find_map(|line| line.strip_prefix("in-pack: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("in-pack count");
    assert_eq!(everything.len(), git_count + 1);

    let target = tmp.path().join("target.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", target.to_str().expect("utf-8")],
    );
    let target_odb = Odb::new(&target.join("objects"));
    let source = layered.odb().read_store();
    for oid in &everything {
        let obj = source.read(oid).expect("read").expect("listed object");
        assert_eq!(target_odb.write(obj.kind, &obj.data).expect("copy"), *oid);
    }
    git(
        &target,
        &["update-ref", "refs/heads/main", &new_tip.to_hex()],
    );
    git(&target, &["fsck", "--strict", "--full", "--no-dangling"]);
    assert_eq!(git(&target, &["rev-list", "--count", "main"]).trim(), "7");
}

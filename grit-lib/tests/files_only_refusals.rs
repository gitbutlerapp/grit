//! Every files-only API refuses a custom primary with the exact
//! [`FilesOnlyOperation`] it is documented under.

use std::ops::ControlFlow;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::maintenance::{
    self, CommitGraphOptions, DropPromisedBlobsOptions, GcOptions, MidxOptions, OffloadOptions,
    PruneOptions, RepackOptions,
};
use grit_lib::objects::index_pack::{install_pack_bytes, install_pack_path, IngestPackOptions};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{Odb, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::progress::CancelToken;
use grit_lib::refs::gc::prune_loose_unreachable;
use grit_lib::repo::error::FilesOnlyOperation;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use time::OffsetDateTime;

/// A writable store that cannot delete objects: the default
/// [`WritableObjectStore::as_prunable`] of `None`.
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

fn memory_repo() -> (tempfile::TempDir, Repository) {
    custom_repo(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
}

fn unprunable_repo() -> (tempfile::TempDir, Repository) {
    custom_repo(Arc::new(UnprunableStore(MemoryStore::new(HashAlgo::Sha1))))
}

fn custom_repo(store: Arc<dyn WritableObjectStore>) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty()
            .with_object_store(store)
            .with_work_tree(dir.path()),
    )
    .expect("open");
    (dir, repo)
}

#[track_caller]
fn assert_refused<T: std::fmt::Debug>(
    result: grit_lib::error::Result<T>,
    expected: FilesOnlyOperation,
) {
    match result {
        Err(Error::Repository(RepositoryError::UnsupportedObjectStore { operation })) => {
            assert_eq!(operation, expected);
        }
        other => panic!("expected UnsupportedObjectStore({expected:?}), got {other:?}"),
    }
}

#[test]
fn odb_gc_without_prune_capability_refuses_with_prune() {
    let (_dir, repo) = unprunable_repo();
    assert_refused(repo.odb().gc(), FilesOnlyOperation::Prune);
}

#[test]
fn maintenance_gc_without_prune_capability_refuses_with_prune() {
    let (_dir, repo) = unprunable_repo();
    assert_refused(
        maintenance::gc(&repo, &GcOptions::default()),
        FilesOnlyOperation::Prune,
    );
    assert_refused(
        maintenance::gc_with_cancel(&repo, &GcOptions::default(), &CancelToken::never()),
        FilesOnlyOperation::Prune,
    );
}

#[test]
fn prune_loose_objects_without_prune_capability_refuses_with_prune() {
    let (_dir, repo) = unprunable_repo();
    assert_refused(
        maintenance::prune_loose_objects(&repo, &PruneOptions::default()),
        FilesOnlyOperation::Prune,
    );
}

#[test]
fn prune_loose_unreachable_without_prune_capability_refuses_with_prune() {
    let (_dir, repo) = unprunable_repo();
    assert_refused(
        prune_loose_unreachable(repo.odb(), &[], None),
        FilesOnlyOperation::Prune,
    );
}

#[test]
fn repack_refuses_with_repack() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        maintenance::repack(&repo, &RepackOptions::default()),
        FilesOnlyOperation::Repack,
    );
}

#[test]
fn repack_all_refuses_with_repack() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        maintenance::repack_all(&repo, &RepackOptions::default()),
        FilesOnlyOperation::Repack,
    );
    assert_refused(
        maintenance::repack_all_with_cancel(
            &repo,
            &RepackOptions::default(),
            &CancelToken::never(),
        ),
        FilesOnlyOperation::Repack,
    );
}

#[test]
fn commit_graph_write_refuses_with_commit_graph_write() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        repo.odb().write_commit_graph(),
        FilesOnlyOperation::CommitGraphWrite,
    );
    assert_refused(
        maintenance::write_commit_graph(&repo, &CommitGraphOptions::default()),
        FilesOnlyOperation::CommitGraphWrite,
    );
}

#[test]
fn write_midx_refuses_with_midx_write() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        maintenance::write_midx(&repo, &MidxOptions::default()),
        FilesOnlyOperation::MidxWrite,
    );
}

#[test]
fn pack_store_refuses_with_pack_store() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        repo.odb().pack_store().map(|_| ()),
        FilesOnlyOperation::PackStore,
    );
}

#[test]
fn largest_pack_index_path_refuses_with_largest_pack_index() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        repo.odb().largest_pack_index_path(),
        FilesOnlyOperation::LargestPackIndex,
    );
}

#[test]
fn install_pack_refuses_with_pack_install() {
    let (dir, repo) = memory_repo();
    assert_refused(
        install_pack_bytes(Vec::new(), repo.odb(), &IngestPackOptions::default()),
        FilesOnlyOperation::PackInstall,
    );
    let pack = dir.path().join("empty.pack");
    std::fs::write(&pack, b"").expect("write pack");
    assert_refused(
        install_pack_path(&pack, repo.odb(), &IngestPackOptions::default()),
        FilesOnlyOperation::PackInstall,
    );
}

#[test]
fn local_loose_writes_refuse_with_loose_write() {
    let (_dir, repo) = memory_repo();
    let odb = repo.odb();
    assert_refused(
        odb.write_local(ObjectKind::Blob, b"x"),
        FilesOnlyOperation::LooseWrite,
    );
    assert_refused(
        odb.write_raw_local(b"blob 1\0x"),
        FilesOnlyOperation::LooseWrite,
    );
    assert_refused(
        odb.write_loose_materialize(ObjectKind::Blob, b"x"),
        FilesOnlyOperation::LooseWrite,
    );
    odb.write(ObjectKind::Blob, b"x")
        .expect("write works on any primary");
}

#[test]
fn primary_refuses_with_files_source() {
    let (_dir, repo) = memory_repo();
    assert_refused(repo.odb().primary(), FilesOnlyOperation::FilesSource);
    repo.odb()
        .primary_writable()
        .expect("primary_writable works on any primary");
}

#[test]
fn offload_refuses_with_offload() {
    let (_dir, repo) = memory_repo();
    assert_refused(
        maintenance::offload_blobs(&repo, &OffloadOptions::new("lop", 1024)),
        FilesOnlyOperation::Offload,
    );
}

#[test]
fn drop_promised_blobs_refuses_with_drop_large_blobs() {
    let (_dir, repo) = memory_repo();
    let opts = DropPromisedBlobsOptions::new(OffsetDateTime::UNIX_EPOCH, 1024, None);
    assert_refused(
        maintenance::drop_promised_blobs(&repo, &opts),
        FilesOnlyOperation::DropLargeBlobs,
    );
    assert_refused(
        repo.drop_promised_blobs(&opts),
        FilesOnlyOperation::DropLargeBlobs,
    );
}

/// A files-backed `Odb` built without a repository has no git directory to run
/// repository-wide maintenance against, so it refuses the same way.
#[test]
fn standalone_files_odb_refuses_repository_maintenance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("mkdir objects");
    let odb = Odb::new(&objects);
    assert_refused(odb.gc(), FilesOnlyOperation::Gc);
    assert_refused(
        odb.write_commit_graph(),
        FilesOnlyOperation::CommitGraphWrite,
    );
}

#[test]
fn refusal_keeps_its_error_code_and_names_the_operation() {
    let (_dir, repo) = unprunable_repo();
    let err = repo
        .odb()
        .gc()
        .expect_err("gc on a custom primary that cannot delete");
    assert_eq!(err.kind_name(), "unsupported_object_store");
    let message = err.to_string();
    assert!(message.contains("pruning unreachable objects"), "{message}");
    assert!(message.contains("custom"), "{message}");
}

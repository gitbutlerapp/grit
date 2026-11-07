//! `Repository::open` with an `OdbBuilder` or a custom object store accepts a relative git dir.
//!
//! This binary holds a single test because it changes the process working directory.

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

#[test]
fn odb_builder_and_custom_store_resolve_objects_dir_from_relative_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    Repository::init(&root, &InitOptions::default()).expect("init");
    std::env::set_current_dir(dir.path()).expect("chdir");
    let canonical_objects = root
        .join(".git/objects")
        .canonicalize()
        .expect("canonical objects");

    let repo = Repository::open(
        Path::new("repo/.git"),
        &(RepositoryOptions::empty()
            .with_odb_builder(OdbBuilder::new())
            .with_work_tree(Path::new("repo"))),
    )
    .expect("relative git dir with read layers");
    assert_eq!(repo.odb().objects_dir(), canonical_objects);
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"relative\n")
        .expect("write");
    assert!(root
        .join(".git/objects")
        .join(&blob.to_hex()[..2])
        .join(&blob.to_hex()[2..])
        .is_file());

    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let custom = Repository::open(
        Path::new("repo/.git"),
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(Path::new("repo"))),
    )
    .expect("a custom primary with a relative git dir");
    assert_eq!(custom.odb().objects_dir(), canonical_objects);
    let oid = custom
        .odb()
        .write(ObjectKind::Blob, b"in memory\n")
        .expect("write");
    assert_eq!(custom.odb().read(&oid).expect("read").data, b"in memory\n");
    assert!(!root
        .join(".git/objects")
        .join(&oid.to_hex()[..2])
        .join(&oid.to_hex()[2..])
        .exists());
}

//! `RepositoryBuilder` with an `OdbBuilder` accepts relative paths.
//!
//! This binary holds a single test because it changes the process working directory.

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

#[test]
fn odb_builder_relative_objects_dir_matches_relative_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    Repository::init(&root, &InitOptions::default()).expect("init");
    std::env::set_current_dir(dir.path()).expect("chdir");

    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(OdbBuilder::files("repo/.git/objects"))
        .open(Path::new("repo/.git"), Some(Path::new("repo")))
        .expect("relative objects dir and git dir name the same repository");
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"relative\n")
        .expect("write");
    assert!(root
        .join(".git/objects")
        .join(&blob.to_hex()[..2])
        .join(&blob.to_hex()[2..])
        .is_file());

    let mismatch = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(OdbBuilder::files("elsewhere/objects"))
        .open(Path::new("repo/.git"), Some(Path::new("repo")));
    assert!(matches!(mismatch, Err(Error::PathError(_))), "{mismatch:?}");

    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let custom = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files("not/a/real/objects")
                .primary(store.clone())
                .alternates(false),
        )
        .open(Path::new("repo/.git"), Some(Path::new("repo")))
        .expect("a custom primary ignores the builder's objects dir");
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

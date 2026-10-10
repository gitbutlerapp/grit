//! Regression tests for FilesRefStore namespace context and refname validation.

use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, FilesRefStore, FilesRefStoreConfig, RawRef, RefStore, RefStoreError, RefTransaction,
    RefUpdate,
};
use grit_lib::refs::LogRefsConfig;

const TRAVERSAL_REF: &str = "refs/heads/../../config";

fn sample_oid() -> ObjectId {
    "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
}

fn bare_git_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().to_path_buf();
    fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
    fs::write(
        git_dir.join("config"),
        "[core]\nrepositoryformatversion = 0\n",
    )
    .expect("config");
    (dir, git_dir)
}

fn open_namespaced(git_dir: &std::path::Path) -> FilesRefStore {
    FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.to_path_buf(),
        common_dir: git_dir.to_path_buf(),
        namespace_prefix: Some("refs/namespaces/acme/".to_owned()),
        log_refs: LogRefsConfig::Normal,
    })
}

#[test]
fn files_store_applies_explicit_namespace_prefix() {
    let (_dir, git_dir) = bare_git_dir();
    let store = open_namespaced(&git_dir);
    let oid = sample_oid();

    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: Some(RawRef::Direct(oid)),
            expected: Expected::Missing,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let namespaced = git_dir.join("refs/namespaces/acme/refs/heads/main");
    let unscoped = git_dir.join("refs/heads/main");
    assert!(
        namespaced.is_file(),
        "expected namespaced storage at {}",
        namespaced.display()
    );
    assert!(
        !unscoped.exists(),
        "logical ref must not write unscoped path {}",
        unscoped.display()
    );
    assert_eq!(
        store.read_raw("refs/heads/main").expect("read"),
        Some(RawRef::Direct(oid))
    );
}

#[test]
fn files_store_rejects_traversal_refname_before_io() {
    let (_dir, git_dir) = bare_git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let config_before = fs::read(git_dir.join("config")).expect("config");

    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: TRAVERSAL_REF.to_owned(),
            new_value: Some(RawRef::Direct(sample_oid())),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");

    let err = match store.prepare(txn) {
        Err(err) => err,
        Ok(_) => panic!("must reject traversal refname"),
    };
    assert!(
        matches!(err, RefStoreError::InvalidRefName { .. }),
        "{err:?}"
    );
    assert_eq!(
        fs::read(git_dir.join("config")).expect("config"),
        config_before,
        "repository config must not be touched"
    );
}

#[test]
fn files_store_rejects_invalid_refname_corpus() {
    let samples = [
        "refs/heads/../../config",
        "refs/heads/../../../config",
        "refs/heads/.",
        "refs/heads/..",
    ];
    let (_dir, git_dir) = bare_git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");

    for name in samples {
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: name.to_owned(),
                new_value: Some(RawRef::Direct(sample_oid())),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .expect("txn");
        let err = match store.prepare(txn) {
            Err(err) => err,
            Ok(_) => panic!("reject bad name {name}"),
        };
        assert!(
            matches!(err, RefStoreError::InvalidRefName { .. }),
            "name {name}: {err:?}"
        );
    }
}

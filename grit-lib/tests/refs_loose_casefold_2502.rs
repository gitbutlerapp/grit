//! Issue #2502: creating one ref with `core.ignorecase` must not read every loose ref body.

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, FilesRefStore, FilesRefStoreConfig, RawRef, RefStore, RefStoreError, RefTransaction,
    RefUpdate,
};
use grit_lib::refs::{write_packed_refs_file, PackedRefRecord};
use grit_test_support::{git, rev_parse};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn repo_with_many_loose_tags(dir: &Path, count: usize) -> (PathBuf, ObjectId) {
    git(dir, &["init", "-q"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    let git_dir = dir.join(".git");
    git(&git_dir, &["config", "core.ignorecase", "true"]);
    let tip = rev_parse(dir, "HEAD");
    for i in 1..=count {
        git(
            &git_dir,
            &["update-ref", &format!("refs/tags/n{i}"), &tip.to_hex()],
        );
    }
    (git_dir, tip)
}

fn prepare_branch_zz(store: &FilesRefStore, tip: ObjectId) {
    let update = RefUpdate {
        name: "refs/heads/zz".to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Missing,
        reflog: None,
        flags: Default::default(),
    };
    let txn = RefTransaction::new().update(update).expect("valid update");
    store.prepare(txn).expect("prepare branch create");
}

#[test]
fn create_branch_with_many_loose_tags_stays_fast() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (git_dir, tip) = repo_with_many_loose_tags(dir.path(), 2_000);

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    });

    let start = Instant::now();
    prepare_branch_zz(&store, tip);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "branch create took {elapsed:?} with 2000 loose tags (expected well under linear ref scan)"
    );
}

#[test]
fn create_tag_with_many_loose_tags_still_rejects_casefold_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (git_dir, tip) = repo_with_many_loose_tags(dir.path(), 100);
    git(&git_dir, &["update-ref", "refs/tags/Foo", &tip.to_hex()]);

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    });
    let update = RefUpdate {
        name: "refs/tags/foo".to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Missing,
        reflog: None,
        flags: Default::default(),
    };
    let txn = RefTransaction::new().update(update).expect("valid update");
    assert!(matches!(
        store.prepare(txn),
        Err(RefStoreError::CreateExists { ref name }) if name == "refs/tags/foo"
    ));
}

#[test]
fn create_rejects_loose_nested_casefold_path_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (git_dir, tip) = repo_with_many_loose_tags(dir.path(), 0);
    git(
        &git_dir,
        &["update-ref", "refs/heads/Foo/bar", &tip.to_hex()],
    );

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    });
    let update = RefUpdate {
        name: "refs/heads/foo/baz".to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Missing,
        reflog: None,
        flags: Default::default(),
    };
    let txn = RefTransaction::new().update(update).expect("valid update");
    assert!(
        store.prepare(txn).is_err(),
        "parallel Foo/ vs foo/ trees must be rejected under ignorecase"
    );
}

#[test]
fn create_rejects_packed_nested_casefold_path_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
    let git_dir = dir.path().join(".git");
    git(&git_dir, &["config", "core.ignorecase", "true"]);
    let tip = rev_parse(dir.path(), "HEAD");
    write_packed_refs_file(
        &git_dir,
        &[PackedRefRecord {
            refname: "refs/heads/Foo/bar".to_owned(),
            oid: tip,
            peeled: None,
        }],
    )
    .expect("packed-refs");

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    });
    let update = RefUpdate {
        name: "refs/heads/foo/baz".to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Missing,
        reflog: None,
        flags: Default::default(),
    };
    let txn = RefTransaction::new().update(update).expect("valid update");
    assert!(matches!(
        store.prepare(txn),
        Err(RefStoreError::CreateExists { ref name }) if name == "refs/heads/foo/baz"
    ));
}

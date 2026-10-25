//! Switching away from a branch must not fail because the current index lists a hostile path.

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_hash_object_literally, HashAlgo};
use std::fs;

fn write_blob(repo_git: &std::path::Path, data: &[u8]) -> String {
    git_hash_object_literally(repo_git, HashAlgo::Sha1, "blob", data).expect("blob")
}

#[test]
fn switch_away_when_index_lists_hostile_path_not_in_target_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = repo.work_tree().unwrap();
    let git_dir = repo.git_dir();

    fs::write(wt.join("safe.txt"), b"safe\n").unwrap();
    git(wt, &["add", "safe.txt"]);
    git(wt, &["commit", "-m", "main"]);

    fs::write(wt.join("feature.txt"), b"f\n").unwrap();
    git(wt, &["checkout", "-b", "feature"]);
    git(wt, &["add", "feature.txt"]);
    git(wt, &["commit", "-m", "feature"]);

    git(wt, &["checkout", "main"]);

    let blob = write_blob(git_dir, b"hostile");
    let blob_oid: ObjectId = blob.parse().expect("oid");
    let mut index = Index::load(&repo.index_handle().path()).expect("index");
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: blob_oid,
        flags: 4,
        flags_extended: None,
        path: b".git".to_vec(),
        base_index_pos: 0,
    });
    let index_path = repo.index_handle().path();
    index.write(&index_path).expect("write index");

    let mode = resolve_switch_target(&repo, "feature").expect("resolve feature");
    switch_repository(
        &repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("switch despite hostile index path");

    assert!(wt.join("feature.txt").exists());
    let index = Index::load(&repo.index_handle().path()).expect("index after switch");
    assert!(
        index.get(b"feature.txt", 0).is_some(),
        "feature tree should be checked out"
    );
}

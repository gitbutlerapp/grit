//! Stash pop rolls back when untracked parent blobs cannot be read.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::stash::{flatten_tree_full, StashApplyOptions, StashMode, StashPushOptions};
use grit_test_support::{git, git_cmd};
use std::fs;
use tempfile::tempdir;

fn stash_push_untracked() -> StashPushOptions {
    StashPushOptions {
        message: Some("wip".to_owned()),
        include_untracked: true,
        identity: "T <t@e.com> 1000000000 +0000".parse().expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: StashMode::default(),
    }
}

#[test]
fn pop_rolls_back_when_untracked_blob_missing() {
    let dir = tempdir().unwrap();
    git_cmd(&["init", "-q", "-b", "main"])
        .in_dir(dir.path())
        .exec();
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "T"]);
    fs::write(dir.path().join("tracked.txt"), b"head\n").unwrap();
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "-qm", "seed"]);
    fs::write(dir.path().join("tracked.txt"), b"stashed\n").unwrap();
    fs::write(dir.path().join("only-untracked.txt"), b"ut\n").unwrap();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).unwrap();
    repo.stash()
        .push(
            &stash_push_untracked(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap()
        .unwrap();

    let stash_oid =
        grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash").expect("stash ref");
    let stash_commit = repo.odb().read_commit(&stash_oid).unwrap();
    assert!(stash_commit.parents.len() >= 3, "expected untracked parent");
    let ut_tree = repo.odb().commit_tree(&stash_commit.parents[2]).unwrap();
    let protect = grit_lib::path_validate::ProtectOptions::default();
    let ut_entries = flatten_tree_full(repo.odb(), &ut_tree, "", protect).unwrap();
    let ut_blob = ut_entries
        .iter()
        .find(|e| e.path == "only-untracked.txt")
        .expect("untracked path in U tree")
        .oid;
    let hex = ut_blob.to_hex();
    let loose = repo
        .git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::remove_file(&loose).expect("remove untracked blob");

    let index_before = fs::read(repo.git_dir().join("index")).unwrap();
    let tracked_before = fs::read(dir.path().join("tracked.txt")).unwrap();

    let err = repo
        .stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("not found") || format!("{err}").contains("object"),
        "pop must fail on missing blob, got {err}"
    );

    assert_eq!(
        fs::read(dir.path().join("tracked.txt")).unwrap(),
        tracked_before,
        "tracked file must roll back"
    );
    assert_eq!(
        fs::read(repo.git_dir().join("index")).unwrap(),
        index_before,
        "index must be unchanged"
    );
    assert!(
        grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash")
            .ok()
            .is_some(),
        "stash entry must remain"
    );
}

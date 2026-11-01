//! `grit switch -c` from a remote-tracking start point sets upstream like `git switch`.

use grit_test_support::{git, grit_cmd};

#[test]
fn switch_create_from_remote_tracking_branch_sets_upstream() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("up.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let seed = tmp.path().join("seed");
    git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), "seed"]);
    std::fs::write(seed.join("a.txt"), "a\n").expect("write");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "one"]);
    git(&seed, &["push", "-q", "origin", "main"]);

    let clone = tmp.path().join("c");
    grit_cmd(&[
        "clone",
        "-q",
        bare.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(tmp.path())
    .suc();
    grit_cmd(&["switch", "-c", "work", "origin/main"])
        .in_dir(&clone)
        .suc();

    let status = git(&clone, &["status", "-sb"]);
    assert!(
        status.starts_with("## work...origin/main"),
        "expected upstream in status, got: {status}"
    );
    assert_eq!(
        git(&clone, &["config", "--get", "branch.work.remote"]).trim(),
        "origin"
    );
    assert_eq!(
        git(&clone, &["config", "--get", "branch.work.merge"]).trim(),
        "refs/heads/main"
    );
}

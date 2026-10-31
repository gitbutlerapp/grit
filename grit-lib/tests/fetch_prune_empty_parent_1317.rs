//! Regression for GitHub issue #1317: pruning a nested remote-tracking ref must
//! remove empty parent directories under `refs/remotes/<remote>/`, so a later
//! fetch can create a sibling ref at the same path (e.g. `feature/x` pruned,
//! then `feature` appears on the remote).

use std::fs;

use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode};
use grit_test_support::git;

#[test]
fn fetch_prune_removes_empty_parent_then_fetch_succeeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let work = tmp.path().join("work");
    let local = tmp.path().join("clone");

    fs::create_dir_all(&remote).expect("remote dir");
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    fs::create_dir_all(&work).expect("work dir");
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["config", "user.name", "x"]);
    git(&work, &["config", "user.email", "x@x"]);
    fs::write(work.join("a.txt"), "a\n").expect("write a.txt");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "c1"]);
    git(&work, &["branch", "feature/x"]);
    git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf8 path"),
        ],
    );
    git(&work, &["push", "-q", "origin", "main", "feature/x"]);

    fs::create_dir_all(&local).expect("local dir");
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let wildcard = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::None;
        o
    };
    fetch_local(&local_git, &remote, &wildcard).expect("initial fetch");
    resolve_ref(&local_git, "refs/remotes/origin/feature/x").expect("nested tracking ref");

    git(&work, &["push", "-q", "origin", "--delete", "feature/x"]);

    let mut pruning = wildcard.clone();
    pruning.prune = true;
    fetch_local(&local_git, &remote, &pruning).expect("prune fetch");
    assert!(
        resolve_ref(&local_git, "refs/remotes/origin/feature/x").is_err(),
        "nested tracking ref must be gone"
    );

    let feature_parent = local_git.join("refs/remotes/origin/feature");
    assert!(
        !feature_parent.exists(),
        "empty parent directory must not remain at {}",
        feature_parent.display()
    );

    git(&work, &["push", "-q", "origin", "main:feature"]);

    fetch_local(&local_git, &remote, &wildcard)
        .expect("fetch after remote republished feature at former parent path");
    resolve_ref(&local_git, "refs/remotes/origin/feature")
        .expect("new origin/feature tracking ref");
}

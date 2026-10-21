//! CLI commits carry stable change ids checked with system git.

use std::fs;

use grit_test_support::{git, grit, unique_tmp};

fn change_id_from_cat(cat: &str) -> Option<String> {
    cat.lines()
        .find(|l| l.starts_with("change-id "))
        .map(|l| l.strip_prefix("change-id ").unwrap().to_owned())
}

#[test]
fn cli_commit_and_amend_keep_change_id() {
    let dir = unique_tmp("commit-change-id", "cli");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    std::fs::write(dir.join("a.txt"), b"1\n").unwrap();

    assert!(grit(&dir, &["commit", "-m", "first"]).ok());

    std::fs::write(dir.join("a.txt"), b"2\n").unwrap();
    assert!(grit(&dir, &["commit", "-m", "second"]).ok());
    let tip_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let tip_id = change_id_from_cat(&tip_cat).expect("tip change-id");

    assert!(grit(&dir, &["commit", "--amend", "-m", "second amended"]).ok());

    git(&dir, &["fsck", "--strict"]);
    let amended_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let amended_id = change_id_from_cat(&amended_cat).expect("amended change-id");
    assert_eq!(amended_id, tip_id);
}

#[test]
fn pull_merge_commit_gets_change_id() {
    let scratch = unique_tmp("pull-merge-change-id", "cli");
    let seed = scratch.join("seed");
    let remote = scratch.join("remote.git");
    let clone = scratch.join("clone");
    fs::create_dir_all(&seed).unwrap();

    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["config", "user.name", "T"]);
    git(&seed, &["config", "user.email", "t@e.com"]);
    fs::write(seed.join("README.md"), b"seed\n").unwrap();
    assert!(grit(&seed, &["commit", "-m", "seed"]).ok());

    git(&scratch, &["init", "--bare", "remote.git"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    assert!(grit(&seed, &["push"]).ok());

    git(&scratch, &["clone", remote.to_str().unwrap(), "clone"]);
    fs::write(clone.join("clone.txt"), b"clone\n").unwrap();
    assert!(grit(&clone, &["commit", "-m", "clone work"]).ok());
    assert!(grit(&clone, &["push"]).ok());

    fs::write(seed.join("seed.txt"), b"diverge\n").unwrap();
    assert!(grit(&seed, &["commit", "-m", "seed diverge"]).ok());
    assert!(grit(&seed, &["fetch"]).ok());
    assert!(grit(&seed, &["pull"]).ok());

    git(&seed, &["fsck", "--strict"]);
    let cat = git(&seed, &["cat-file", "-p", "HEAD"]);
    assert!(
        change_id_from_cat(&cat).is_some(),
        "merge pull commit should carry change-id:\n{cat}"
    );
}

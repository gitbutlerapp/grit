//! Exit codes and integrity reporting for `grit fsck`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::{git_hash_object_literally, CliFixture, HashAlgo};

fn grit_fsck(fixture: &CliFixture, extra: &[&str]) -> i32 {
    let mut args = vec!["fsck"];
    args.extend(extra);
    fixture.run(&args).status.unwrap_or(1)
}

#[test]
fn fsck_cli_exit_codes() {
    let clean = CliFixture::new();
    assert_eq!(grit_fsck(&clean, &[]), 0, "clean repo should exit 0");

    let dangling = CliFixture::empty();
    let wt = dangling.path();
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("hash-object");
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(b"unreferenced\n").expect("write");
    }
    let out = child.wait_with_output().expect("hash-object wait");
    assert!(out.status.success(), "hash-object: {:?}", out.stderr);
    assert_eq!(
        grit_fsck(&dangling, &[]),
        0,
        "dangling object alone should exit 0"
    );

    let corrupt = CliFixture::empty();
    one_commit_with_staged_blob(&corrupt);
    let repo = Repository::discover(Some(corrupt.path()), &RepositoryOptions::default())
        .expect("discover");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged_oid = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"missing.txt"))
        .expect("entry")
        .oid;
    let loose = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == staged_oid)
        .expect("loose path");
    fs::remove_file(loose.1).expect("remove blob");

    assert_eq!(
        grit_fsck(&corrupt, &[]),
        1,
        "missing index blob should exit 1"
    );
}

#[test]
fn fsck_missing_tagger_warning_exits_zero_like_git() {
    let fix = CliFixture::new();
    let wt = fix.path();
    let head = std::process::Command::new("git")
        .current_dir(wt)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(head.status.success());
    let commit = String::from_utf8(head.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let tag_body = format!("object {commit}\ntype commit\ntag v-no-tagger\n\nannotated\n");
    let tag_oid = git_hash_object_literally(wt, HashAlgo::Sha1, "tag", tag_body.as_bytes())
        .expect("write tag literally");
    let update = std::process::Command::new("git")
        .current_dir(wt)
        .args(["update-ref", "refs/tags/ann", &tag_oid])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("update-ref");
    assert!(update.success());

    let git_fsck = std::process::Command::new("git")
        .current_dir(wt)
        .args(["fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git fsck")
        .code()
        .unwrap_or(1);
    assert_eq!(git_fsck, 0, "git fsck should exit 0 for missingTaggerEntry");
    assert_eq!(
        grit_fsck(&fix, &[]),
        0,
        "grit fsck should exit 0 for warning-class missingTaggerEntry"
    );
    assert_eq!(
        grit_fsck(&fix, &["--json"]),
        0,
        "grit fsck --json should exit 0 for missingTaggerEntry"
    );
    assert_eq!(
        grit_fsck(&fix, &["--strict"]),
        0,
        "strict should not promote INFO-level missingTaggerEntry to a failing exit"
    );
}

fn one_commit_with_staged_blob(fixture: &CliFixture) {
    let repo = Repository::discover(Some(fixture.path()), &RepositoryOptions::default())
        .expect("discover");
    fs::write(repo.work_tree().expect("wt").join("missing.txt"), b"x").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
}

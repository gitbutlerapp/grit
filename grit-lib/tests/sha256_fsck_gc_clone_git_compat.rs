//! Regression for issue #1962: SHA-256 repos from git must fsck/gc/clone cleanly.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::maintenance::{gc, GcOptions};
use grit_lib::objects::HashAlgo;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::reflog::all_reflog_oids;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;
use grit_test_support::git_supports_sha256;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init_sha256(dir: &Path) {
    git(
        dir,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
}

#[test]
fn git_sha256_repo_reflog_null_oid_excluded_from_fsck_and_gc() {
    if !git_supports_sha256() {
        return;
    }
    let td = TempDir::new().expect("tempdir");
    git_init_sha256(td.path());
    std::fs::write(td.path().join("a"), "hi\n").unwrap();
    git(td.path(), &["add", "a"]);
    git(td.path(), &["commit", "-qm", "one"]);

    let repo =
        Repository::discover(Some(td.path()), &RepositoryOptions::empty()).expect("open");
    let null_hex = "0".repeat(HashAlgo::Sha256.hex_len());
    let oids = all_reflog_oids(repo.git_dir()).expect("reflog oids");
    assert!(
        !oids.iter().any(|o| o.to_hex() == null_hex),
        "64-zero null oid must not appear in reflog reachability roots"
    );

    let report = repo
        .fsck(&FsckOptions::default(), &mut NullProgress, &NeverCancel)
        .expect("fsck");
    assert!(
        report.issues.is_empty(),
        "fsck should be clean: {:?}",
        report.issues
    );

    gc(&repo, &GcOptions::default()).expect("gc");
}

#[test]
fn grit_clone_local_git_sha256_repo() {
    if !git_supports_sha256() {
        return;
    }
    let parent = TempDir::new().expect("tempdir");
    let source = parent.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git_init_sha256(&source);
    std::fs::write(source.join("a"), "hi\n").unwrap();
    git(&source, &["add", "a"]);
    git(&source, &["commit", "-qm", "one"]);

    let dest = parent.path().join("grit-clone");
    let outcome = clone(
        CloneOptions {
            url: source.to_string_lossy().into_owned(),
            dest: dest.clone(),
            ..CloneOptions::new(
                source.to_string_lossy().into_owned(),
                dest.clone(),
            )
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone");
    assert_eq!(outcome.repo.odb().hash_algo(), HashAlgo::Sha256);

    let expected = Command::new("git")
        .current_dir(&source)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(expected.status.success());
    let expected = String::from_utf8_lossy(&expected.stdout).trim().to_owned();

    let cloned = Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("cloned rev-parse");
    assert!(cloned.status.success());
    assert_eq!(String::from_utf8_lossy(&cloned.stdout).trim(), expected);

    git(&dest, &["fsck", "--strict"]);
}

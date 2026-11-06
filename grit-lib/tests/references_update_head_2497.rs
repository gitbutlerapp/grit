//! Issue #2497: [`References::update`] must follow `HEAD` symrefs and write reflogs.

#[allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::zero_oid;
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::objects::NewCommit;
use grit_lib::refs::append_reflog;
use grit_lib::refs::store::{RefName, RefStoreError};
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

const REFLOG_ID: &str = "T <t@example.com> 1700000000 +0000";

fn init_repo(dir: &Path) -> Repository {
    let repo =
        Repository::init(dir, &InitOptions::default().with_initial_branch("main")).expect("init");
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    repo
}

fn reopen(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::from_process()).expect("reopen")
}

fn disable_log_all_ref_updates(dir: &Path) {
    git(dir, &["config", "core.logAllRefUpdates", "false"]);
}

fn log_file(dir: &Path, refname: &str) -> PathBuf {
    dir.join(".git/logs").join(refname)
}

fn reflog_lines(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    fs::read_to_string(path)
        .expect("read log")
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

fn issue_repro(repo: &Repository) -> (grit_lib::objects::ObjectId, grit_lib::objects::ObjectId) {
    let who = Signature::new(
        "A",
        "a@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("sig");
    let tree = repo.objects().write_tree([]).expect("tree");
    let c1 = repo
        .objects()
        .write_commit(&NewCommit::new(tree, who.clone(), who.clone(), "one\n"))
        .expect("c1");
    let c2 = repo
        .objects()
        .write_commit(&NewCommit::new(tree, who.clone(), who, "two\n").with_parents([c1]))
        .expect("c2");
    write_ref(repo.git_dir(), "refs/heads/main", &c1).expect("seed main");
    (c1, c2)
}

#[test]
fn update_head_follows_symref_and_writes_reflogs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    let head = RefName::try_from("HEAD").expect("HEAD");
    let main = RefName::try_from("refs/heads/main").expect("main");

    repo.references().update(&head, &c2).expect("update HEAD");

    let head_file = fs::read_to_string(dir.path().join(".git/HEAD")).expect("HEAD file");
    assert_eq!(head_file.trim(), "ref: refs/heads/main");
    assert_eq!(repo.references().resolve(&main).expect("main"), c2);
    assert_eq!(repo.references().resolve(&head).expect("HEAD"), c2);
    assert_ne!(c1, c2);

    let logs = dir.path().join(".git/logs");
    assert!(logs.join("HEAD").exists(), "HEAD reflog missing");
    assert!(
        logs.join("refs/heads/main").exists(),
        "branch reflog missing"
    );

    git(dir.path(), &["fsck"]);
    let head_log = git(dir.path(), &["reflog", "show", "-1", "--format=%H", "HEAD"]);
    let branch_log = git(
        dir.path(),
        &["reflog", "show", "-1", "--format=%H", "refs/heads/main"],
    );
    assert_eq!(head_log.trim(), c2.to_hex());
    assert_eq!(branch_log.trim(), c2.to_hex());
}

#[test]
fn update_other_branch_creates_reflog_under_normal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    let other = RefName::try_from("refs/heads/other").expect("other");
    repo.references().update(&other, &c2).expect("create other");

    assert!(
        dir.path().join(".git/logs/refs/heads/other").exists(),
        "expected reflog for new branch"
    );
    git(dir.path(), &["fsck"]);

    let _ = c1;
}

#[test]
fn log_all_ref_updates_false_skips_new_reflog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (_c1, c2) = issue_repro(&repo);

    let cfg = dir.path().join(".git/config");
    let mut text = fs::read_to_string(&cfg).expect("config");
    if !text.contains("logAllRefUpdates") {
        text.push_str("\n[core]\n\tlogAllRefUpdates = false\n");
        fs::write(&cfg, text).expect("write config");
    }

    let other = RefName::try_from("refs/heads/other").expect("other");
    repo.references().update(&other, &c2).expect("update");
    assert!(
        !dir.path().join(".git/logs/refs/heads/other").exists(),
        "reflog should not be auto-created"
    );
}

#[test]
fn update_direct_detaches_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    let head = RefName::try_from("HEAD").expect("HEAD");
    let main = RefName::try_from("refs/heads/main").expect("main");

    repo.references().update_direct(&head, &c2).expect("detach");

    assert_eq!(
        fs::read_to_string(dir.path().join(".git/HEAD"))
            .expect("HEAD")
            .trim(),
        c2.to_hex()
    );
    assert_eq!(repo.references().resolve(&main).expect("main"), c1);
    assert!(dir.path().join(".git/logs/HEAD").exists());
}

#[test]
fn update_head_with_only_head_reflog_does_not_touch_branch_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    disable_log_all_ref_updates(dir.path());
    let repo = reopen(dir.path());
    append_reflog(
        repo.git_dir(),
        "HEAD",
        &zero_oid(),
        &c1,
        REFLOG_ID,
        "seed head",
        true,
    )
    .expect("seed HEAD log");
    assert_eq!(reflog_lines(&log_file(dir.path(), "HEAD")), 1);
    assert_eq!(reflog_lines(&log_file(dir.path(), "refs/heads/main")), 0);

    let head = RefName::try_from("HEAD").expect("HEAD");
    repo.references().update(&head, &c2).expect("update HEAD");

    assert_eq!(reflog_lines(&log_file(dir.path(), "HEAD")), 2);
    assert_eq!(
        reflog_lines(&log_file(dir.path(), "refs/heads/main")),
        0,
        "branch log must stay absent when only HEAD had a log"
    );
    git(dir.path(), &["fsck"]);
}

#[test]
fn update_head_with_only_branch_reflog_does_not_touch_head_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    disable_log_all_ref_updates(dir.path());
    let repo = reopen(dir.path());
    append_reflog(
        repo.git_dir(),
        "refs/heads/main",
        &zero_oid(),
        &c1,
        REFLOG_ID,
        "seed branch",
        true,
    )
    .expect("seed branch log");
    assert_eq!(reflog_lines(&log_file(dir.path(), "refs/heads/main")), 1);
    assert_eq!(reflog_lines(&log_file(dir.path(), "HEAD")), 0);

    let head = RefName::try_from("HEAD").expect("HEAD");
    repo.references().update(&head, &c2).expect("update HEAD");

    assert_eq!(reflog_lines(&log_file(dir.path(), "refs/heads/main")), 2);
    assert_eq!(
        reflog_lines(&log_file(dir.path(), "HEAD")),
        0,
        "HEAD log must stay absent when only the branch had a log"
    );
    git(dir.path(), &["fsck"]);
}

#[test]
fn compare_and_swap_refuses_stale_tip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (c1, c2) = issue_repro(&repo);
    let main = RefName::try_from("refs/heads/main").expect("main");

    let err = repo
        .references()
        .compare_and_swap(&main, &c2, &c1)
        .expect_err("stale");
    assert!(matches!(
        err,
        Error::RefStore(RefStoreError::ExpectedMismatch { .. })
    ));
    assert_eq!(repo.references().resolve(&main).expect("main"), c1);
}

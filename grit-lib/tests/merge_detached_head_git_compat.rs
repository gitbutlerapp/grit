//! Merge into detached `HEAD` — compatibility with system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::merge::{
    abort_branch_merge, integrate_detached_head_with, ContinueOptions, FastForwardPolicy,
    IntegrateBranchOutcome, MergeCommitMode, MergeOptions,
};
use grit_lib::porcelain::merge::{continue_merge, integrate_detached_head};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::process::Command;

fn grit_repo(tmp: &tempfile::TempDir) -> Repository {
    Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open grit repo")
}

fn setup_identities(path: &std::path::Path) {
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "Test"]);
}

fn detached_ff_repo() -> (tempfile::TempDir, ObjectId, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    std::fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main");
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("g.txt"), "b\n").expect("write");
    git(tmp.path(), &["add", "g.txt"]);
    git(tmp.path(), &["commit", "-m", "topic"]);
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp.path(), &["switch", "main"]);
    git(tmp.path(), &["switch", "--detach", "HEAD"]);
    (tmp, main, topic, main)
}

fn git_fsck(path: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(path)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn head_oid(path: &std::path::Path) -> ObjectId {
    git(path, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD")
}

#[test]
fn fast_forward_moves_only_head_and_matches_git() {
    let (tmp, detached_base, topic, main_tip) = detached_ff_repo();
    assert_eq!(head_oid(tmp.path()), detached_base);
    let repo = grit_repo(&tmp);
    let outcome = integrate_detached_head(
        &repo,
        detached_base,
        topic,
        "topic",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { oid, .. } if oid == topic
    ));
    assert_eq!(head_oid(tmp.path()), topic);
    assert_eq!(
        git(tmp.path(), &["rev-parse", "main"]).trim(),
        main_tip.to_hex()
    );
    git_fsck(tmp.path());
}

#[test]
fn no_ff_merge_commit_on_detached_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    git(tmp.path(), &["config", "grit.changeIds", "true"]);
    std::fs::write(tmp.path().join("shared.txt"), "base\n").expect("write");
    git(tmp.path(), &["add", "shared.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("shared.txt"), "topic\n").expect("write");
    git(tmp.path(), &["add", "shared.txt"]);
    git(tmp.path(), &["commit", "-m", "topic"]);
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp.path(), &["switch", "main"]);
    std::fs::write(tmp.path().join("main-only.txt"), "main\n").expect("write");
    git(tmp.path(), &["add", "main-only.txt"]);
    git(tmp.path(), &["commit", "-m", "main"]);
    let main_before = git(tmp.path(), &["rev-parse", "main"]).trim().to_owned();
    let into_oid = head_oid(tmp.path());
    git(tmp.path(), &["switch", "--detach", "HEAD"]);

    let repo = grit_repo(&tmp);
    let outcome = integrate_detached_head_with(
        &repo,
        into_oid,
        topic,
        "topic",
        &MergeOptions {
            fast_forward: FastForwardPolicy::Never,
            ..MergeOptions::default()
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge");
    let IntegrateBranchOutcome::Merged { oid, change_id } = outcome else {
        panic!("expected merge commit, got {outcome:?}");
    };
    assert!(change_id.is_some());
    let obj = repo.odb().read(&oid).expect("read merge");
    let commit = parse_commit(&obj.data).expect("parse");
    assert_eq!(commit.parents.len(), 2);
    assert_eq!(head_oid(tmp.path()), oid);
    assert_eq!(git(tmp.path(), &["rev-parse", "main"]).trim(), main_before);
    git_fsck(tmp.path());
}

#[test]
fn conflict_then_continue_and_abort_on_detached_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    std::fs::write(tmp.path().join("f"), "base\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-qm", "base"]);
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("f"), "theirs\n").expect("write");
    git(tmp.path(), &["commit", "-qam", "topic"]);
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp.path(), &["switch", "main"]);
    std::fs::write(tmp.path().join("f"), "ours\n").expect("write");
    git(tmp.path(), &["commit", "-qam", "main"]);
    let into_oid = head_oid(tmp.path());
    git(tmp.path(), &["switch", "--detach", "HEAD"]);
    let head_at_conflict_start = into_oid;

    let repo = grit_repo(&tmp);
    let outcome = integrate_detached_head(
        &repo,
        into_oid,
        topic,
        "topic",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert_eq!(head_oid(tmp.path()), head_at_conflict_start);

    std::fs::write(tmp.path().join("f"), "resolved\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    let id = "Merge Test <merge@example.com> 1700000000 +0000".to_owned();
    let cont = continue_merge(
        &repo,
        &ContinueOptions {
            message: None,
            author: Some(id.parse().expect("sig")),
            committer: Some(id.parse().expect("sig")),
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
    assert_ne!(cont.oid, head_at_conflict_start);
    assert_eq!(head_oid(tmp.path()), cont.oid);
    git_fsck(tmp.path());

    // Abort path on a fresh conflicted detached merge.
    let tmp2 = tempfile::tempdir().expect("tempdir2");
    git(tmp2.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp2.path());
    std::fs::write(tmp2.path().join("f"), "base\n").expect("write");
    git(tmp2.path(), &["add", "f"]);
    git(tmp2.path(), &["commit", "-qm", "base"]);
    git(tmp2.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp2.path().join("f"), "theirs\n").expect("write");
    git(tmp2.path(), &["commit", "-qam", "topic"]);
    let topic2: ObjectId = git(tmp2.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp2.path(), &["switch", "main"]);
    std::fs::write(tmp2.path().join("f"), "ours\n").expect("write");
    git(tmp2.path(), &["commit", "-qam", "main"]);
    let into2 = head_oid(tmp2.path());
    git(tmp2.path(), &["switch", "--detach", "HEAD"]);
    let repo2 = grit_repo(&tmp2);
    integrate_detached_head(
        &repo2,
        into2,
        topic2,
        "topic",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge");
    abort_branch_merge(&repo2).expect("abort");
    assert_eq!(head_oid(tmp2.path()), into2);
    assert!(!repo2.git_dir().join("MERGE_HEAD").exists());
}

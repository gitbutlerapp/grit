//! In-memory merge dry-run planning (`plan_branch_merge`).

use grit_lib::merge::{FastForwardPolicy, MergeDryRunPlan, MergeOptions};
use grit_lib::repo::Repository;
use grit_test_support::plan_branch_merge;
use grit_test_support::{git, git_dir_state_digest, git_fsck};
use tempfile::TempDir;

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(
        Some(path),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("repo")
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    tmp
}

fn commit_all(dir: &TempDir, file: &str, contents: &str, msg: &str) -> grit_lib::objects::ObjectId {
    std::fs::write(dir.path().join(file), contents).expect("write");
    git(dir.path(), &["add", file]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            msg,
        ],
    );
    git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

#[test]
fn merge_dry_run_plan_up_to_date() {
    let dir = init_repo();
    let tip = commit_all(&dir, "a.txt", "a\n", "one");
    let repo = open_repo(dir.path());
    let plan = plan_branch_merge(&repo, tip, tip, "main", &MergeOptions::default()).expect("plan");
    assert_eq!(plan, MergeDryRunPlan::UpToDate);
}

#[test]
fn merge_dry_run_plan_ff() {
    let dir = init_repo();
    let base = commit_all(&dir, "a.txt", "a\n", "base");
    git(dir.path(), &["checkout", "-b", "topic"]);
    commit_all(&dir, "b.txt", "b\n", "topic");
    let topic = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("topic");
    git(dir.path(), &["checkout", "main"]);
    let repo = open_repo(dir.path());
    let plan =
        plan_branch_merge(&repo, base, topic, "topic", &MergeOptions::default()).expect("plan");
    assert_eq!(
        plan,
        MergeDryRunPlan::FastForward {
            tip: topic,
            change_id: None,
        }
    );
}

#[test]
fn merge_dry_run_plan_merge() {
    let dir = init_repo();
    commit_all(&dir, "a.txt", "base\n", "base");
    git(dir.path(), &["checkout", "-b", "topic"]);
    std::fs::write(dir.path().join("a.txt"), "side\n").expect("write");
    git(dir.path(), &["commit", "-am", "side"]);
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("b.txt"), "main\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);
    git(dir.path(), &["commit", "-m", "main"]);
    let main = git(dir.path(), &["rev-parse", "refs/heads/main"])
        .trim()
        .parse()
        .expect("main");
    let topic = git(dir.path(), &["rev-parse", "refs/heads/topic"])
        .trim()
        .parse()
        .expect("topic");
    let repo = open_repo(dir.path());
    let plan =
        plan_branch_merge(&repo, main, topic, "topic", &MergeOptions::default()).expect("plan");
    assert_eq!(
        plan,
        MergeDryRunPlan::MergeCommit {
            parents: vec![main, topic],
        }
    );
}

#[test]
fn merge_dry_run_plan_conflict() {
    let dir = init_repo();
    commit_all(&dir, "a.txt", "base\n", "base");
    git(dir.path(), &["checkout", "-b", "topic"]);
    std::fs::write(dir.path().join("a.txt"), "side\n").expect("write");
    git(dir.path(), &["commit", "-am", "side"]);
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("a.txt"), "main\n").expect("write");
    git(dir.path(), &["commit", "-am", "main"]);
    let main = git(dir.path(), &["rev-parse", "refs/heads/main"])
        .trim()
        .parse()
        .expect("main");
    let topic = git(dir.path(), &["rev-parse", "refs/heads/topic"])
        .trim()
        .parse()
        .expect("topic");
    let repo = open_repo(dir.path());
    let plan =
        plan_branch_merge(&repo, main, topic, "topic", &MergeOptions::default()).expect("plan");
    match plan {
        MergeDryRunPlan::Conflicts { conflicts } => {
            assert!(conflicts.iter().any(|c| c.path == "a.txt"));
        }
        other => panic!("expected conflicts, got {other:?}"),
    }
}

#[test]
fn plan_branch_merge_writes_nothing() {
    let dir = init_repo();
    commit_all(&dir, "f.txt", "1\n", "base");
    git(dir.path(), &["checkout", "-b", "topic"]);
    std::fs::write(dir.path().join("f.txt"), "2\n").expect("write");
    git(dir.path(), &["commit", "-am", "topic"]);
    let topic = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("topic");
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("g.txt"), "m\n").expect("write");
    git(dir.path(), &["add", "g.txt"]);
    git(dir.path(), &["commit", "-m", "main"]);
    let main = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    let git_dir = dir.path().join(".git");
    let before = git_dir_state_digest(&git_dir);
    let repo = open_repo(dir.path());
    plan_branch_merge(
        &repo,
        main,
        topic,
        "topic",
        &MergeOptions::default().with_fast_forward(FastForwardPolicy::Never),
    )
    .expect("plan");
    assert_eq!(before, git_dir_state_digest(&git_dir));
    let fsck = git_fsck(dir.path(), true);
    assert!(fsck.ok, "fsck: {:?}", fsck.msg_ids);
}

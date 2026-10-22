//! `grit switch` flags: `-C`, `--orphan`, `--discard-changes`, track, and `--no-guess`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::branch::TrackMode;
use grit_lib::porcelain::switch::{
    resolve_create_branch_with, resolve_orphan_branch, resolve_switch_target_with_guess,
    switch_repository, SwitchOptions,
};
use grit_lib::refs;
use grit_lib::refs::reflog::read_reflog;
use grit_lib::repo::Repository;
use std::process::Command;
use tempfile::TempDir;

fn init_repo() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    for args in [
        ["config", "user.email", "a@b.c"],
        ["config", "user.name", "A"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("git config");
    }
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    (dir, repo)
}

fn git_commit(dir: &std::path::Path, path: &str, contents: &str) {
    std::fs::write(dir.join(path), contents).expect("write");
    Command::new("git")
        .args(["add", path])
        .current_dir(dir)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "c"])
        .current_dir(dir)
        .status()
        .expect("commit");
}

#[test]
fn switch_force_create_matches_git_reflog_and_ref() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    Command::new("git")
        .args(["branch", "topic"])
        .current_dir(dir.path())
        .status()
        .expect("branch");
    git_commit(dir.path(), "g", "2\n");
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("rev-parse");
    let head_oid = String::from_utf8_lossy(&head.stdout).trim().to_owned();

    Command::new("git")
        .args(["switch", "-C", "topic", "HEAD~1"])
        .current_dir(dir.path())
        .status()
        .expect("git switch -C");
    let git_topic = Command::new("git")
        .args(["rev-parse", "topic"])
        .current_dir(dir.path())
        .output()
        .expect("git topic");
    let git_reflog = read_reflog(repo.git_dir(), "refs/heads/topic").expect("git-side reflog");

    Command::new("git")
        .args(["switch", "-C", "main", &head_oid])
        .current_dir(dir.path())
        .status()
        .expect("restore main");

    let mode = resolve_create_branch_with(&repo, "topic", Some("HEAD~1"), true, TrackMode::Default)
        .expect("resolve -C");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit -C");

    let grit_topic = Command::new("git")
        .args(["rev-parse", "topic"])
        .current_dir(dir.path())
        .output()
        .expect("grit topic");
    assert_eq!(
        String::from_utf8_lossy(&git_topic.stdout).trim(),
        String::from_utf8_lossy(&grit_topic.stdout).trim()
    );

    let grit_reflog = read_reflog(repo.git_dir(), "refs/heads/topic").expect("grit reflog");
    assert_eq!(git_reflog.len(), grit_reflog.len());
    for (g, r) in git_reflog.iter().zip(grit_reflog.iter()) {
        assert_eq!(g.new_oid, r.new_oid);
        assert_eq!(g.message, r.message);
    }
}

#[test]
fn switch_orphan_matches_git_porcelain_v2() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    std::fs::write(dir.path().join("u"), "u\n").expect("untracked");

    Command::new("git")
        .args(["switch", "--orphan", "newb"])
        .current_dir(dir.path())
        .status()
        .expect("git orphan");
    let git_status = Command::new("git")
        .args(["status", "--porcelain=v2", "-b"])
        .current_dir(dir.path())
        .output()
        .expect("git status");
    assert!(git_status.status.success());

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("back to main");
    std::fs::write(dir.path().join("u"), "u\n").expect("untracked again");

    let mode = resolve_orphan_branch(&repo, "newb").expect("orphan mode");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit orphan");

    let grit_status = Command::new("git")
        .args(["status", "--porcelain=v2", "-b"])
        .current_dir(dir.path())
        .output()
        .expect("status after grit");
    assert_eq!(git_status.stdout, grit_status.stdout);
}

#[test]
fn switch_discard_changes_leaves_clean_git_status() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    Command::new("git")
        .args(["switch", "-c", "other"])
        .current_dir(dir.path())
        .status()
        .expect("other");
    git_commit(dir.path(), "f", "2\n");
    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    std::fs::write(dir.path().join("f"), "dirty\n").expect("dirty");

    let mode = resolve_switch_target_with_guess(&repo, "other", true).expect("other");
    switch_repository(
        &repo,
        &SwitchOptions {
            mode,
            discard_changes: true,
        },
    )
    .expect("discard switch");

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(status.stdout.is_empty(), "worktree should be clean");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f")).expect("read"),
        "2\n"
    );
}

#[test]
fn switch_track_and_no_track_match_git_config() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    let main_oid = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
    refs::write_ref(repo.git_dir(), "refs/remotes/origin/main", &main_oid).expect("remote");

    Command::new("git")
        .args(["switch", "-c", "tracked", "-t", "origin/main"])
        .current_dir(dir.path())
        .status()
        .expect("git track");
    let git_tracked_remote = Command::new("git")
        .args(["config", "--get", "branch.tracked.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let git_tracked_merge = Command::new("git")
        .args(["config", "--get", "branch.tracked.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "tracked"])
        .current_dir(dir.path())
        .status()
        .expect("delete");

    let mode = resolve_create_branch_with(
        &repo,
        "tracked",
        Some("origin/main"),
        false,
        TrackMode::Track,
    )
    .expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit track");
    let grit_tracked_remote = Command::new("git")
        .args(["config", "--get", "branch.tracked.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let grit_tracked_merge = Command::new("git")
        .args(["config", "--get", "branch.tracked.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    assert_eq!(git_tracked_remote.stdout, grit_tracked_remote.stdout);
    assert_eq!(git_tracked_merge.stdout, grit_tracked_merge.stdout);

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "tracked"])
        .current_dir(dir.path())
        .status()
        .expect("delete");

    Command::new("git")
        .args(["switch", "-c", "notrack", "--no-track", "origin/main"])
        .current_dir(dir.path())
        .status()
        .expect("git no-track");
    assert!(
        !dir.path().join(".git/config").exists() || {
            Command::new("git")
                .args(["config", "--get", "branch.notrack.remote"])
                .current_dir(dir.path())
                .output()
                .expect("cfg")
                .status
                .success()
                == false
        }
    );

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "notrack"])
        .current_dir(dir.path())
        .status()
        .expect("delete notrack");

    let mode = resolve_create_branch_with(
        &repo,
        "notrack",
        Some("origin/main"),
        false,
        TrackMode::NoTrack,
    )
    .expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit no-track");
    let out = Command::new("git")
        .args(["config", "--get", "branch.notrack.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    assert!(!out.status.success());
}

#[test]
fn switch_no_guess_fails_when_only_remote_topic_exists() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    refs::write_ref(
        repo.git_dir(),
        "refs/remotes/origin/topic",
        &grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
    )
    .expect("remote");

    assert!(!Command::new("git")
        .args(["-c", "checkout.guess=false", "switch", "topic"])
        .current_dir(dir.path())
        .status()
        .expect("git")
        .success());

    let err = resolve_switch_target_with_guess(&repo, "topic", false).expect_err("no guess");
    assert!(err.to_string().contains("topic"));
}

#[test]
fn switch_minus_round_trips_previous_branch() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    Command::new("git")
        .args(["switch", "-c", "topic"])
        .current_dir(dir.path())
        .status()
        .expect("topic");
    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");

    let to_topic = resolve_switch_target_with_guess(&repo, "-", true).expect("@{-1}");
    switch_repository(&repo, &SwitchOptions::new(to_topic)).expect("switch -");
    let to_main = resolve_switch_target_with_guess(&repo, "-", true).expect("@{-1}");
    switch_repository(&repo, &SwitchOptions::new(to_main)).expect("switch - again");

    let head = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("head");
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");
}

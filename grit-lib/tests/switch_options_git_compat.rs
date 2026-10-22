//! `grit switch` flags: `-C`, `--orphan`, `--discard-changes`, track, and `--no-guess`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::branch_error::TrackMode;
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

    let mode = resolve_create_branch_with(&repo, "topic", Some("HEAD~1"), true, TrackMode::Auto)
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

struct OrphanOracleSnapshot {
    staged_file_bytes: Vec<u8>,
    ls_files_stage: Vec<u8>,
    symbolic_head: Vec<u8>,
    rev_parse_head_failed: bool,
    status_porcelain_v2: Vec<u8>,
}

fn capture_orphan_oracle(dir: &std::path::Path, staged_rel: &str) -> OrphanOracleSnapshot {
    let staged_file_bytes = std::fs::read(dir.join(staged_rel)).unwrap_or_default();
    let ls_files_stage = Command::new("git")
        .args(["ls-files", "--stage"])
        .current_dir(dir)
        .output()
        .expect("ls-files")
        .stdout;
    let symbolic_head = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("symref")
        .stdout;
    let rev_parse = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("rev-parse");
    let status_porcelain_v2 = Command::new("git")
        .args(["status", "--porcelain=v2"])
        .current_dir(dir)
        .output()
        .expect("status")
        .stdout;
    OrphanOracleSnapshot {
        staged_file_bytes,
        ls_files_stage,
        symbolic_head,
        rev_parse_head_failed: !rev_parse.status.success(),
        status_porcelain_v2,
    }
}

fn init_repo_with_staged_add(dir: &std::path::Path, staged_rel: &str, staged_body: &str) {
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .status()
        .expect("init");
    for args in [
        ["config", "user.email", "a@b.c"],
        ["config", "user.name", "A"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("config");
    }
    std::fs::write(dir.join("base.txt"), "base-content\n").expect("base");
    Command::new("git")
        .args(["add", "base.txt"])
        .current_dir(dir)
        .status()
        .expect("add base");
    Command::new("git")
        .args(["commit", "-qm", "base"])
        .current_dir(dir)
        .status()
        .expect("commit base");
    std::fs::write(dir.join(staged_rel), staged_body).expect("staged write");
    Command::new("git")
        .args(["add", staged_rel])
        .current_dir(dir)
        .status()
        .expect("add staged");
}

#[test]
fn switch_orphan_preserves_staged_additions_like_git() {
    const STAGED: &str = "staged_add.txt";
    const BODY: &str = "unique-staged-oracle-content\n";

    let git_dir = tempfile::tempdir().expect("git dir");
    init_repo_with_staged_add(git_dir.path(), STAGED, BODY);
    const BRANCH: &str = "orphan_oracle";
    Command::new("git")
        .args(["switch", "--orphan", BRANCH])
        .current_dir(git_dir.path())
        .status()
        .expect("git orphan");
    let git_oracle = capture_orphan_oracle(git_dir.path(), STAGED);
    assert!(git_oracle.rev_parse_head_failed);

    let grit_dir = tempfile::tempdir().expect("grit dir");
    init_repo_with_staged_add(grit_dir.path(), STAGED, BODY);
    let repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    let mode = resolve_orphan_branch(&repo, BRANCH).expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit orphan");
    let grit_oracle = capture_orphan_oracle(grit_dir.path(), STAGED);

    assert_eq!(git_oracle.staged_file_bytes, grit_oracle.staged_file_bytes);
    assert_eq!(git_oracle.ls_files_stage, grit_oracle.ls_files_stage);
    assert_eq!(git_oracle.symbolic_head, grit_oracle.symbolic_head);
    assert_eq!(
        git_oracle.rev_parse_head_failed,
        grit_oracle.rev_parse_head_failed
    );
    assert_eq!(
        git_oracle.status_porcelain_v2,
        grit_oracle.status_porcelain_v2
    );
    assert!(!git_oracle.staged_file_bytes.is_empty());
    assert!(
        git_oracle
            .ls_files_stage
            .windows(STAGED.len())
            .any(|w| w == STAGED.as_bytes()),
        "git must keep staged path in index"
    );
}

#[test]
fn switch_orphan_staged_add_with_unstaged_edits_matches_git() {
    const STAGED: &str = "staged_add.txt";

    let git_dir = tempfile::tempdir().expect("git dir");
    init_repo_with_staged_add(git_dir.path(), STAGED, "staged-line\n");
    std::fs::write(git_dir.path().join(STAGED), "staged-line\nextra-unstaged\n").expect("dirty");
    const BRANCH: &str = "orphan_oracle";
    Command::new("git")
        .args(["switch", "--orphan", BRANCH])
        .current_dir(git_dir.path())
        .status()
        .expect("git orphan");
    let git_oracle = capture_orphan_oracle(git_dir.path(), STAGED);

    let grit_dir = tempfile::tempdir().expect("grit dir");
    init_repo_with_staged_add(grit_dir.path(), STAGED, "staged-line\n");
    std::fs::write(
        grit_dir.path().join(STAGED),
        "staged-line\nextra-unstaged\n",
    )
    .expect("dirty");
    let repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    let mode = resolve_orphan_branch(&repo, BRANCH).expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit orphan");
    let grit_oracle = capture_orphan_oracle(grit_dir.path(), STAGED);

    assert_eq!(git_oracle.staged_file_bytes, grit_oracle.staged_file_bytes);
    assert_eq!(git_oracle.ls_files_stage, grit_oracle.ls_files_stage);
    assert_eq!(
        git_oracle.status_porcelain_v2,
        grit_oracle.status_porcelain_v2
    );
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

    let mode =
        resolve_switch_target_with_guess(&repo, "other", true, TrackMode::Auto).expect("other");
    switch_repository(
        &repo,
        &SwitchOptions {
            mode,
            discard_changes: true,
            dry_run: false,
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
fn switch_force_create_checked_out_branch_matches_git_worktree() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    git_commit(dir.path(), "f", "2\n");
    let tip = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("tip");
    let tip_hex = String::from_utf8_lossy(&tip.stdout).trim().to_owned();

    Command::new("git")
        .args(["switch", "-C", "main", "HEAD~1"])
        .current_dir(dir.path())
        .status()
        .expect("git -C main");

    let git_status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(git_status.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f")).expect("read"),
        "1\n"
    );

    Command::new("git")
        .args(["reset", "--hard", &tip_hex])
        .current_dir(dir.path())
        .status()
        .expect("restore main tip");

    let mode = resolve_create_branch_with(&repo, "main", Some("HEAD~1"), true, TrackMode::Auto)
        .expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit -C main");

    let grit_status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(grit_status.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f")).expect("read"),
        "1\n"
    );
}

#[test]
fn switch_force_create_failed_checkout_leaves_branch_ref() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    git_commit(dir.path(), "f", "2\n");
    let tip = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("tip");
    std::fs::write(dir.path().join("f"), "dirty\n").expect("dirty");

    let reflog_before = read_reflog(repo.git_dir(), "refs/heads/main").expect("reflog before");
    let mode = resolve_create_branch_with(&repo, "main", Some("HEAD~1"), true, TrackMode::Auto)
        .expect("resolve");
    let err = switch_repository(&repo, &SwitchOptions::new(mode)).expect_err("conflict");
    assert!(matches!(err, grit_lib::error::Error::Message(_)));
    assert_eq!(
        grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
        tip
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f")).expect("read"),
        "dirty\n"
    );
    let reflog_after = read_reflog(repo.git_dir(), "refs/heads/main").expect("reflog after");
    assert_eq!(
        reflog_after.len(),
        reflog_before.len(),
        "branch ref reflog must be unchanged on failed -C checkout"
    );
}

#[test]
fn switch_orphan_refuses_dirty_tracked_file() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    std::fs::write(dir.path().join("f"), "dirty\n").expect("dirty");

    assert!(!Command::new("git")
        .args(["switch", "--orphan", "newb"])
        .current_dir(dir.path())
        .status()
        .expect("git")
        .success());

    let mode = resolve_orphan_branch(&repo, "newb").expect("orphan");
    let err = switch_repository(&repo, &SwitchOptions::new(mode)).expect_err("dirty");
    assert!(matches!(err, grit_lib::error::Error::Message(_)));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f")).expect("read"),
        "dirty\n"
    );
    assert!(Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("head")
        .stdout
        .starts_with(b"main"));
}

#[test]
fn switch_track_local_name_differs_from_remote_branch() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    let bare = tempfile::tempdir().expect("bare");
    Command::new("git")
        .args(["init", "--bare", "-q"])
        .current_dir(bare.path())
        .status()
        .expect("bare");
    Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ])
        .current_dir(dir.path())
        .status()
        .expect("remote");
    Command::new("git")
        .args(["push", "-q", "origin", "main"])
        .current_dir(dir.path())
        .status()
        .expect("push main");
    std::fs::write(dir.path().join("g"), "2\n").expect("g");
    Command::new("git")
        .args(["add", "g"])
        .current_dir(dir.path())
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "g"])
        .current_dir(dir.path())
        .status()
        .expect("commit");
    Command::new("git")
        .args(["push", "-q", "origin", "HEAD:refs/heads/bar"])
        .current_dir(dir.path())
        .status()
        .expect("push bar");
    Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(dir.path())
        .status()
        .expect("fetch");

    let git_out = Command::new("git")
        .args(["switch", "-c", "foo", "-t", "origin/bar"])
        .current_dir(dir.path())
        .output()
        .expect("git");
    assert!(
        git_out.status.success(),
        "git: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_remote = Command::new("git")
        .args(["config", "--get", "branch.foo.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let git_merge = Command::new("git")
        .args(["config", "--get", "branch.foo.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "foo"])
        .current_dir(dir.path())
        .status()
        .expect("del");

    let mode =
        resolve_create_branch_with(&repo, "foo", Some("origin/bar"), false, TrackMode::Always)
            .expect("resolve");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit");
    let grit_remote = Command::new("git")
        .args(["config", "--get", "branch.foo.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let grit_merge = Command::new("git")
        .args(["config", "--get", "branch.foo.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    assert_eq!(git_remote.stdout, grit_remote.stdout);
    assert_eq!(git_merge.stdout, grit_merge.stdout);
    assert_eq!(
        String::from_utf8_lossy(&git_merge.stdout).trim(),
        "refs/heads/bar"
    );
}

#[test]
fn switch_dwim_no_track_skips_upstream_config() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    let bare = tempfile::tempdir().expect("bare");
    Command::new("git")
        .args(["init", "--bare", "-q"])
        .current_dir(bare.path())
        .status()
        .expect("bare");
    Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ])
        .current_dir(dir.path())
        .status()
        .expect("remote");
    Command::new("git")
        .args(["push", "-q", "origin", "main"])
        .current_dir(dir.path())
        .status()
        .expect("push");
    Command::new("git")
        .args(["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .current_dir(dir.path())
        .status()
        .expect("push topic");
    Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(dir.path())
        .status()
        .expect("fetch");

    let git_no = Command::new("git")
        .args(["switch", "-c", "topic", "--no-track", "origin/topic"])
        .current_dir(dir.path())
        .output()
        .expect("git");
    assert!(
        git_no.status.success(),
        "{}",
        String::from_utf8_lossy(&git_no.stderr)
    );

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "topic"])
        .current_dir(dir.path())
        .status()
        .expect("del");

    let mode =
        resolve_switch_target_with_guess(&repo, "topic", true, TrackMode::Never).expect("dwim");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit");
    assert!(!Command::new("git")
        .args(["config", "--get", "branch.topic.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg")
        .status
        .success());
}

#[test]
fn switch_track_and_no_track_match_git_config() {
    let (dir, repo) = init_repo();
    git_commit(dir.path(), "f", "1\n");
    let bare = tempfile::tempdir().expect("bare");
    Command::new("git")
        .args(["init", "--bare", "-q"])
        .current_dir(bare.path())
        .status()
        .expect("bare init");
    Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ])
        .current_dir(dir.path())
        .status()
        .expect("remote add");
    Command::new("git")
        .args(["push", "-q", "origin", "main"])
        .current_dir(dir.path())
        .status()
        .expect("push");
    Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(dir.path())
        .status()
        .expect("fetch");

    let git_track = Command::new("git")
        .args(["switch", "-c", "tracked", "-t", "origin/main"])
        .current_dir(dir.path())
        .output()
        .expect("git track");
    assert!(
        git_track.status.success(),
        "git track failed: {}",
        String::from_utf8_lossy(&git_track.stderr)
    );
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
        TrackMode::Always,
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
        .args(["branch", "-D", "feature/topic"])
        .current_dir(dir.path())
        .status()
        .ok();
    Command::new("git")
        .args(["push", "-q", "origin", "HEAD:refs/heads/feature/topic"])
        .current_dir(dir.path())
        .status()
        .expect("push feature/topic");
    Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(dir.path())
        .status()
        .expect("fetch feature/topic");

    let git_slash = Command::new("git")
        .args(["switch", "-c", "tracked", "-t", "origin/feature/topic"])
        .current_dir(dir.path())
        .output()
        .expect("git track slash branch");
    assert!(
        git_slash.status.success(),
        "git switch -t origin/feature/topic failed: {}",
        String::from_utf8_lossy(&git_slash.stderr)
    );
    let git_slash_remote = Command::new("git")
        .args(["config", "--get", "branch.tracked.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let git_slash_merge = Command::new("git")
        .args(["config", "--get", "branch.tracked.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    assert_eq!(
        String::from_utf8_lossy(&git_slash_remote.stdout).trim(),
        "origin"
    );
    assert_eq!(
        String::from_utf8_lossy(&git_slash_merge.stdout).trim(),
        "refs/heads/feature/topic"
    );

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("main");
    Command::new("git")
        .args(["branch", "-D", "tracked"])
        .current_dir(dir.path())
        .status()
        .expect("delete tracked");

    let mode = resolve_create_branch_with(
        &repo,
        "tracked",
        Some("origin/feature/topic"),
        false,
        TrackMode::Always,
    )
    .expect("resolve slash track");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit track slash");
    let grit_slash_remote = Command::new("git")
        .args(["config", "--get", "branch.tracked.remote"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    let grit_slash_merge = Command::new("git")
        .args(["config", "--get", "branch.tracked.merge"])
        .current_dir(dir.path())
        .output()
        .expect("cfg");
    assert_eq!(git_slash_remote.stdout, grit_slash_remote.stdout);
    assert_eq!(git_slash_merge.stdout, grit_slash_merge.stdout);

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

    let git_no_track = Command::new("git")
        .args(["switch", "-c", "notrack", "--no-track", "origin/main"])
        .current_dir(dir.path())
        .output()
        .expect("git no-track");
    assert!(
        git_no_track.status.success(),
        "git no-track failed: {}",
        String::from_utf8_lossy(&git_no_track.stderr)
    );
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
        TrackMode::Never,
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

    let err = resolve_switch_target_with_guess(&repo, "topic", false, TrackMode::Auto)
        .expect_err("no guess");
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

    let to_topic =
        resolve_switch_target_with_guess(&repo, "-", true, TrackMode::Auto).expect("@{-1}");
    switch_repository(&repo, &SwitchOptions::new(to_topic)).expect("switch -");
    let to_main =
        resolve_switch_target_with_guess(&repo, "-", true, TrackMode::Auto).expect("@{-1}");
    switch_repository(&repo, &SwitchOptions::new(to_main)).expect("switch - again");

    let head = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("head");
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");
}

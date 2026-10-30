//! Remote-tracking ref reflogs on fetch/clone vs system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fsck(dir: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["fsck", "--no-dangling"])
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

struct BareRemote {
    work: PathBuf,
    bare: PathBuf,
}

impl BareRemote {
    fn new(tmp: &Path) -> Self {
        let work = tmp.join("work");
        let bare = tmp.join("remote.git");
        git(tmp, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
        git(
            tmp,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        Self { work, bare }
    }

    fn commit_and_push(&self, file: &str, contents: &str, message: &str) {
        self.commit_and_push_branch("main", file, contents, message);
    }

    fn commit_and_push_branch(&self, branch: &str, file: &str, contents: &str, message: &str) {
        if branch != "main" {
            git(&self.work, &["checkout", "-q", branch]);
        }
        std::fs::write(self.work.join(file), contents).expect("write");
        git(&self.work, &["add", file]);
        git(&self.work, &["commit", "-qm", message]);
        git(
            &self.work,
            &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
    }
}

fn fetch_opts() -> FetchOptions {
    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.remote_name = Some("origin".to_owned());
    opts.tags = TagMode::None;
    opts
}

fn grit_fetch(repo: &Path, remote_git: &Path) {
    let local_git = repo.join(".git");
    fetch_local(&local_git, remote_git, &fetch_opts()).expect("fetch_local");
}

fn grit_clone(tmp: &Path, remote_git: &Path, name: &str) -> PathBuf {
    let dest = tmp.join(name);
    clone_repo(
        CloneOptions::new(remote_git.to_string_lossy(), dest.clone()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone");
    dest
}

#[test]
fn fetch_fast_forward_reflog_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let git_clone = tmp.path().join("git-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.bare.to_str().unwrap(),
            git_clone.to_str().unwrap(),
        ],
    );
    let grit_repo = grit_clone(tmp.path(), &remote.bare, "grit-clone");

    remote.commit_and_push("a", "1\n2\n", "c2");
    grit_fetch(&grit_repo, &remote.bare);
    git(&git_clone, &["fetch", "-q"]);

    let grit_prev = git(&grit_repo, &["rev-parse", "origin/main@{1}"]);
    let git_prev = git(&git_clone, &["rev-parse", "origin/main@{1}"]);
    assert_eq!(grit_prev, git_prev);

    let grit_log = git(
        &grit_repo,
        &["reflog", "show", "-1", "--format=%gs", "origin/main"],
    );
    assert!(grit_log.contains("fast-forward"), "{grit_log}");
    fsck(&grit_repo);
}

#[test]
fn fetch_forced_update_reflog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");
    let c1 = git(&repo, &["rev-parse", "origin/main"]);

    remote.commit_and_push("a", "1\n2\n", "c2");
    grit_fetch(&repo, &remote.bare);
    let c2 = git(&repo, &["rev-parse", "origin/main"]);
    // Force remote back to c1.
    git(&remote.bare, &["update-ref", "refs/heads/main", &c1]);
    let mut opts = fetch_opts();
    opts.force = true;
    fetch_local(&repo.join(".git"), &remote.bare, &opts).expect("force fetch");

    let log = git(
        &repo,
        &["reflog", "show", "-1", "--format=%gs", "origin/main"],
    );
    assert!(log.contains("forced-update"), "{log}");
    assert_eq!(git(&repo, &["rev-parse", "origin/main@{1}"]), c2);
}

#[test]
fn fetch_new_branch_storing_head_reflog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");

    git(&remote.work, &["checkout", "-q", "-b", "feature"]);
    remote.commit_and_push_branch("feature", "b", "x\n", "on feature");
    grit_fetch(&repo, &remote.bare);

    let log = git(
        &repo,
        &["reflog", "show", "-1", "--format=%gs", "origin/feature"],
    );
    assert!(log.contains("storing head"), "{log}");
}

#[test]
fn fetch_up_to_date_writes_no_reflog_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");
    grit_fetch(&repo, &remote.bare);
    let log_path = repo.join(".git/logs/refs/remotes/origin/main");
    assert!(!log_path.exists(), "up-to-date fetch should not create log");
}

#[test]
fn fetch_no_reflog_when_log_all_ref_updates_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");
    git(&repo, &["config", "core.logAllRefUpdates", "false"]);
    remote.commit_and_push("a", "1\n2\n", "c2");
    grit_fetch(&repo, &remote.bare);
    assert!(
        !repo.join(".git/logs/refs/remotes/origin/main").exists(),
        "logAllRefUpdates=false must not create reflog"
    );
}

#[test]
fn fetch_tag_tracking_ref_has_no_reflog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    git(&remote.work, &["tag", "-a", "v1", "-m", "t"]);
    git(&remote.work, &["push", "-q", "origin", "refs/tags/v1"]);
    let repo = grit_clone(tmp.path(), &remote.bare, "local");
    let mut opts = fetch_opts();
    opts.tags = TagMode::All;
    fetch_local(&repo.join(".git"), &remote.bare, &opts).expect("tags");
    assert!(
        !repo.join(".git/logs/refs/tags/v1").exists(),
        "tags should not get fetch reflogs by default"
    );
}

#[test]
fn fetch_prune_removes_tracking_reflog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");
    git(&remote.work, &["checkout", "-q", "-b", "gone"]);
    remote.commit_and_push_branch("gone", "b", "x\n", "gone commit");
    grit_fetch(&repo, &remote.bare);
    assert!(repo.join(".git/logs/refs/remotes/origin/gone").exists());

    git(&remote.work, &["checkout", "-q", "main"]);
    git(&remote.work, &["push", "-q", "origin", "--delete", "gone"]);
    let mut opts = fetch_opts();
    opts.prune = true;
    fetch_local(&repo.join(".git"), &remote.bare, &opts).expect("prune fetch");
    assert!(
        !repo.join(".git/logs/refs/remotes/origin/gone").exists(),
        "prune should remove reflog file"
    );
}

#[test]
fn clone_writes_only_head_clone_reflog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path());
    remote.commit_and_push("a", "1\n", "c1");
    let repo = grit_clone(tmp.path(), &remote.bare, "local");

    let logs_dir = repo.join(".git/logs/refs/remotes/origin");
    let entries: Vec<_> = std::fs::read_dir(&logs_dir)
        .expect("logs dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["HEAD".to_owned()]);
    let head_log = git(
        &repo,
        &["reflog", "show", "-1", "--format=%gs", "origin/HEAD"],
    );
    assert!(head_log.starts_with("clone: from"), "{head_log}");
    assert!(
        !repo.join(".git/logs/refs/remotes/origin/main").exists(),
        "clone should not write branch tracking reflogs"
    );
    fsck(&repo);
}

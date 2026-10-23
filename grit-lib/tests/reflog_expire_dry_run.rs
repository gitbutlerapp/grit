//! `RepositoryReflogs::expire` dry-run must not touch log bytes or mtimes and must match git.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::append_reflog;
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::ExpireOptions;
use tempfile::TempDir;

mod support;

use support::copy_worktree;

fn hermetic_git(cmd: &mut Command) {
    cmd.env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git(worktree: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree).args(args);
    hermetic_git(&mut cmd);
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn collect_log_files(dir: &Path) -> Vec<PathBuf> {
    let logs = dir.join(".git/logs");
    let mut files = Vec::new();
    collect_log_files_rec(&logs, &mut files);
    files.sort();
    files
}

fn collect_log_files_rec(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_log_files_rec(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn log_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime)> {
    collect_log_files(dir)
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).expect("read log");
            let mtime = fs::metadata(&path)
                .expect("meta")
                .modified()
                .expect("mtime");
            (path, bytes, mtime)
        })
        .collect()
}

fn git_dry_run_pruned_for_ref(worktree: &Path, refname: &str) -> usize {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree).args([
        "reflog",
        "expire",
        "--dry-run",
        "--verbose",
        "--expire=1600000000",
        "--expire-unreachable=never",
        refname,
    ]);
    hermetic_git(&mut cmd);
    let out = cmd.output().expect("git dry expire");
    assert!(
        out.status.success(),
        "git dry expire {refname}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    combined
        .lines()
        .filter(|line| line.starts_with("prune"))
        .count()
}

#[test]
fn reflog_expire_dry_run() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");

    fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);
    let oid1: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .parse()
        .expect("oid");
    fs::write(tmp.path().join("f.txt"), "b\n").expect("write");
    git(tmp.path(), &["commit", "-am", "c2"]);
    let oid2: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .parse()
        .expect("oid");

    let refname = "refs/heads/main";
    let git_dir = repo.git_dir();
    append_reflog(
        git_dir,
        refname,
        &ObjectId::zero(),
        &oid1,
        "T <t@e.com> 1000000 +0000",
        "old",
        false,
    )
    .expect("log1");
    append_reflog(
        git_dir,
        refname,
        &oid1,
        &oid2,
        "T <t@e.com> 2500000000 +0000",
        "new",
        false,
    )
    .expect("log2");
    write_ref(git_dir, refname, &oid2).expect("tip");

    let grit_copy = TempDir::new().expect("grit copy");
    let git_copy = TempDir::new().expect("git copy");
    copy_worktree(tmp.path(), grit_copy.path());
    copy_worktree(tmp.path(), git_copy.path());

    let grit_repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::default()).expect("open");
    let before = log_snapshot(grit_copy.path());

    let opts = ExpireOptions {
        now: 3_000_000,
        expire: Some("1600000000".into()),
        expire_unreachable: Some("never".into()),
        all: false,
        dry_run: true,
    };

    let dry = grit_repo.reflogs().expire(&opts).expect("dry expire");

    let after_dry = log_snapshot(grit_copy.path());
    assert_eq!(
        before, after_dry,
        "dry run must not change log bytes or mtimes"
    );

    assert_eq!(dry.entries_pruned, 1);
    assert!(dry.dry_run);
    assert_eq!(
        dry.refs
            .iter()
            .find(|r| r.ref_name == refname)
            .map(|r| r.pruned),
        Some(1)
    );

    let real = grit_repo
        .reflogs()
        .expire(&ExpireOptions {
            dry_run: false,
            ..opts.clone()
        })
        .expect("real expire");
    assert_eq!(real.entries_pruned, dry.entries_pruned);
    assert_eq!(
        real.refs
            .iter()
            .find(|r| r.ref_name == refname)
            .map(|r| r.pruned),
        dry.refs
            .iter()
            .find(|r| r.ref_name == refname)
            .map(|r| r.pruned)
    );

    let git_main = git_dry_run_pruned_for_ref(git_copy.path(), refname);
    assert_eq!(
        dry.refs
            .iter()
            .find(|r| r.ref_name == refname)
            .map(|r| r.pruned),
        Some(git_main)
    );
}

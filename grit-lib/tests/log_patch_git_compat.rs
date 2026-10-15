//! `git log -p` patch output parity (Myers default and histogram).

use std::io::Cursor;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    write_commit_log_patch, LogPatchCommitHeader, LogPatchDiffOptions, LogPatchOptions,
};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn init_corpus_repo(git: &std::path::Path, dir: &std::path::Path) {
    git_in(dir, &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.join("README"), b"base\n").unwrap();
    git_in(dir, &["add", "README"]);
    git_in(dir, &["commit", "-q", "-m", "init"]);
    for i in 0..12 {
        std::fs::write(dir.join("README"), format!("line {i}\n")).unwrap();
        git_in(dir, &["add", "README"]);
        git_in(dir, &["commit", "-q", "-m", &format!("change {i}")]);
        if i % 4 == 0 {
            std::fs::write(dir.join(format!("extra-{i}.txt")), b"payload\n").unwrap();
            git_in(dir, &["add", "."]);
            git_in(dir, &["commit", "-q", "-m", &format!("add file {i}")]);
        }
    }
    std::fs::write(dir.join("bin.dat"), &[0u8, 1, 2, 3, 4]).unwrap();
    git_in(dir, &["add", "bin.dat"]);
    git_in(dir, &["commit", "-q", "-m", "binary"]);
}

fn grit_log_patch(repo: &Repository, limit: usize, diff: LogPatchDiffOptions) -> Vec<u8> {
    let opts = RevListOptions {
        max_count: Some(limit),
        ..Default::default()
    };
    let result = rev_list(repo, &["HEAD".to_owned()], &[], &opts).expect("rev-list");
    let patch_opts = LogPatchOptions {
        diff,
        abbrev_len: 7,
        commit_header: LogPatchCommitHeader::FormatPercentH,
    };
    let mut out = Cursor::new(Vec::new());
    for oid in &result.commits {
        repo.odb()
            .with_pack_read_context(|| write_commit_log_patch(repo, &mut out, oid, &patch_opts))
            .expect("patch");
    }
    out.into_inner()
}

#[test]
fn log_patch_corpus_matches_git_default_algorithm() {
    let dir = TempDir::new().expect("tempdir");
    init_corpus_repo(std::path::Path::new("git"), dir.path());
    let git_out = git_in(
        dir.path(),
        &["log", "-p", "--no-color", "--format=%H", "-15"],
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let grit_out = grit_log_patch(&repo, 15, LogPatchDiffOptions::default());
    assert_eq!(grit_out, git_out);
}

#[test]
fn log_patch_corpus_matches_git_histogram() {
    let dir = TempDir::new().expect("tempdir");
    init_corpus_repo(std::path::Path::new("git"), dir.path());
    git_in(dir.path(), &["config", "diff.algorithm", "histogram"]);
    let git_out = git_in(
        dir.path(),
        &["log", "-p", "--no-color", "--format=%H", "-10"],
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let mut diff = LogPatchDiffOptions::default();
    diff.use_git_histogram = true;
    let grit_out = grit_log_patch(&repo, 10, diff);
    assert_eq!(grit_out, git_out);
}

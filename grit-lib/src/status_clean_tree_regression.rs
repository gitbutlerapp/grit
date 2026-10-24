//! Clean-tree `status` must trust matching index stat data without reading file contents
//! (issue #1702), whatever `core.filemode` says and whether the stat scan is serial or parallel.

#![cfg(test)]

use std::fs;
use std::path::Path;
use std::process::Command;

use filetime::{set_file_mtime, FileTime};

use crate::environment::RepositoryOptions;
use crate::porcelain::status::{status, StatusOptions, UntrackedMode};
use crate::progress::NullProgress;
use crate::repo::Repository;
use crate::PARALLEL_STAT_MIN_ENTRIES;

/// Worktree mtime well before the index write, so no entry is racily clean.
const OLD_MTIME: i64 = 1_600_000_000;

fn git_in(dir: &Path, args: &[&str]) {
    // hygiene: git oracle subprocess in test module
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
}

fn git_status_porcelain(dir: &Path) -> String {
    // hygiene: git oracle subprocess in test module
    let out = Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `files` tracked paths spread over nested directories; every eighth file is empty.
fn git_committed_repo(root: &Path, files: usize, filemode: bool) {
    git_in(root, &["init", "-q", "-b", "main", "."]);
    git_in(
        root,
        &[
            "config",
            "core.filemode",
            if filemode { "true" } else { "false" },
        ],
    );
    for i in 0..files {
        let rel = format!("d{}/s{}/f{i:04}.txt", i % 7, i % 3);
        let abs = root.join(&rel);
        fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
        let body = if i % 8 == 0 {
            String::new()
        } else {
            format!("payload {i}\n").repeat(32)
        };
        fs::write(&abs, body).expect("write");
        set_file_mtime(&abs, FileTime::from_unix_time(OLD_MTIME, 0)).expect("mtime");
    }
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-qm", "initial"]);
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo")
}

/// Runs `status` and returns (unstaged paths, worktree blob content reads).
fn status_counting_reads(repo: &Repository, threads: usize) -> (Vec<String>, usize) {
    let options = StatusOptions {
        untracked: UntrackedMode::No,
        stat_parallel_threads: Some(threads),
        ..StatusOptions::default()
    };
    let metrics = repo.odb().hot_path_metrics();
    metrics.reset_blob_content_reads();
    metrics.set_blob_counting(true);
    let model = status(repo, &options, &mut NullProgress).expect("status");
    metrics.set_blob_counting(false);
    let unstaged = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    (unstaged, metrics.blob_content_reads())
}

fn assert_clean_without_content_reads(files: usize, filemode: bool, threads: usize) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, files, filemode);
    assert_eq!(git_status_porcelain(root), "", "git sees a clean tree");
    let repo = open(root);
    for run in 0..2 {
        let (unstaged, reads) = status_counting_reads(&repo, threads);
        assert!(unstaged.is_empty(), "run {run}: unexpected {unstaged:?}");
        assert_eq!(
            reads, 0,
            "run {run}: clean tree (files={files}, filemode={filemode}, threads={threads}) \
             must not read tracked contents"
        );
    }
}

#[test]
fn serial_status_trusts_stat_with_filemode_false() {
    assert_clean_without_content_reads(96, false, 1);
}

#[test]
fn serial_status_trusts_stat_with_filemode_true() {
    assert_clean_without_content_reads(96, true, 1);
}

#[test]
fn parallel_status_trusts_stat_with_filemode_false() {
    assert_clean_without_content_reads(PARALLEL_STAT_MIN_ENTRIES + 24, false, 4);
}

#[test]
fn parallel_status_trusts_stat_with_filemode_true() {
    assert_clean_without_content_reads(PARALLEL_STAT_MIN_ENTRIES + 24, true, 4);
}

#[test]
fn untracked_walk_matches_git_around_submodules_and_tracked_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sub_src = tmp.path().join("sub-src");
    let root = tmp.path().join("super");
    fs::create_dir_all(&sub_src).expect("mkdir");
    fs::create_dir_all(&root).expect("mkdir");
    git_committed_repo(&sub_src, 4, true);
    git_committed_repo(&root, 24, true);
    let sub_url = sub_src.to_str().expect("utf-8 path");
    git_in(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_url,
            "d1/sub",
        ],
    );
    git_in(&root, &["commit", "-qm", "add submodule"]);
    fs::write(root.join("d1/s1/untracked.txt"), "u\n").expect("write");
    fs::write(root.join("d1/sub/inner-untracked.txt"), "u\n").expect("write");
    fs::create_dir_all(root.join("fresh/deeper")).expect("mkdir");
    fs::write(root.join("fresh/deeper/new.txt"), "n\n").expect("write");
    fs::create_dir_all(root.join("d2/s2/embedded")).expect("mkdir");
    git_in(&root.join("d2/s2/embedded"), &["init", "-q"]);

    let repo = open(&root);
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
    let mut untracked = model.untracked.clone();
    untracked.sort();
    assert_eq!(
        untracked,
        vec![
            "d1/s1/untracked.txt".to_owned(),
            "d2/s2/embedded/".to_owned(),
            "fresh/".to_owned()
        ]
    );
    let git_untracked: Vec<String> = git_status_porcelain(&root)
        .lines()
        .filter_map(|l| l.strip_prefix("?? ").map(str::to_owned))
        .collect();
    assert_eq!(untracked, git_untracked);
}

#[test]
fn filemode_false_status_still_hashes_same_size_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, 24, false);
    let rel = "d1/s1/f0001.txt";
    let abs = root.join(rel);
    let original = fs::read(&abs).expect("read");
    let mut edited = original.clone();
    edited[0] = b'P';
    fs::write(&abs, &edited).expect("write");
    set_file_mtime(&abs, FileTime::from_unix_time(OLD_MTIME + 10, 0)).expect("mtime");

    let repo = open(root);
    let (unstaged, reads) = status_counting_reads(&repo, 1);
    assert_eq!(unstaged, vec![rel.to_owned()]);
    // The stat refresh and the diff each hash the edited file; nothing else is read.
    assert!(
        (1..=2).contains(&reads),
        "only the edited file may be read, got {reads} reads"
    );
    assert_eq!(git_status_porcelain(root), format!(" M {rel}\n"));
}

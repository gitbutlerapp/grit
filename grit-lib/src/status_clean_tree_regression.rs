//! Clean-tree `status` must trust matching index stat data without reading file contents
//! (issue #1702), whatever `core.filemode` says and whether the stat scan is serial or parallel.

#![cfg(test)]

use std::fs;
use std::path::Path;
use std::process::Command;

use filetime::{set_file_mtime, FileTime};

use crate::environment::RepositoryOptions;
use crate::porcelain::status::{status, StatusOptions, UntrackedMode};
use crate::progress::{NeverCancel, NullProgress};
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
    let model = status(repo, &options, &mut NullProgress, &NeverCancel).expect("status");
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

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn serial_status_trusts_stat_with_filemode_false() {
    assert_clean_without_content_reads(96, false, 1);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn serial_status_trusts_stat_with_filemode_true() {
    assert_clean_without_content_reads(96, true, 1);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn parallel_status_trusts_stat_with_filemode_false() {
    assert_clean_without_content_reads(PARALLEL_STAT_MIN_ENTRIES + 24, false, 4);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn parallel_status_trusts_stat_with_filemode_true() {
    assert_clean_without_content_reads(PARALLEL_STAT_MIN_ENTRIES + 24, true, 4);
}

/// Pin every tracked file and the index to the same second so all entries are racily clean.
fn make_index_racy(root: &Path, files: usize) {
    for i in 0..files {
        let abs = root.join(format!("d{}/s{}/f{i:04}.txt", i % 7, i % 3));
        set_file_mtime(&abs, FileTime::from_unix_time(OLD_MTIME, 0)).expect("mtime");
    }
    set_file_mtime(
        root.join(".git/index"),
        FileTime::from_unix_time(OLD_MTIME, 500_000_000),
    )
    .expect("index mtime");
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn status_rewrites_racy_index_once_then_trusts_stat() {
    const FILES: usize = 64;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, FILES, true);
    make_index_racy(root, FILES);

    let repo = open(root);
    let (unstaged, first_reads) = status_counting_reads(&repo, 1);
    assert!(unstaged.is_empty(), "{unstaged:?}");
    let non_empty = (0..FILES).filter(|i| i % 8 != 0).count();
    assert!(
        (non_empty..=FILES).contains(&first_reads),
        "racy entries must be verified by content exactly once ({first_reads} reads)"
    );
    let index_mtime = fs::metadata(root.join(".git/index"))
        .and_then(|m| m.modified())
        .expect("index mtime");
    assert!(
        FileTime::from_system_time(index_mtime).unix_seconds() > OLD_MTIME,
        "status must persist the index so its entries stop being racy"
    );

    let (unstaged, second_reads) = status_counting_reads(&repo, 1);
    assert!(unstaged.is_empty(), "{unstaged:?}");
    assert_eq!(second_reads, 0, "second status must trust stat data");
    assert_eq!(git_status_porcelain(root), "");
    git_in(root, &["fsck", "--no-progress"]);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn status_skips_index_rewrite_within_the_second_of_racy_entries() {
    const FILES: usize = 16;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, FILES, true);
    make_index_racy(root, FILES);
    let index_path = root.join(".git/index");
    let before = fs::metadata(&index_path).and_then(|m| m.modified());

    let repo = open(root);
    let options = StatusOptions {
        untracked: UntrackedMode::No,
        now_unix_seconds: Some(OLD_MTIME),
        ..StatusOptions::default()
    };
    let model = status(&repo, &options, &mut NullProgress, &NeverCancel).expect("status");
    assert!(model.unstaged.is_empty());
    assert_eq!(
        fs::metadata(&index_path).and_then(|m| m.modified()).ok(),
        before.ok(),
        "a rewrite in the same second as the racy entries cannot clear them"
    );
}

/// Status right after a checkout: every entry shares the index's timestamp second, so each one
/// must be verified by content once, spread over the worker pool rather than read one by one.
#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn parallel_status_verifies_racy_tree_once_on_worker_threads() {
    const FILES: usize = PARALLEL_STAT_MIN_ENTRIES + 24;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, FILES, true);
    git_in(root, &["config", "index.threads", "4"]);
    make_index_racy(root, FILES);

    let repo = open(root);
    let (unstaged, first_reads) = status_counting_reads(&repo, 4);
    assert!(unstaged.is_empty(), "{unstaged:?}");
    let non_empty = (0..FILES).filter(|i| i % 8 != 0).count();
    assert!(
        (non_empty..=FILES).contains(&first_reads),
        "racy entries must be verified by content exactly once ({first_reads} reads)"
    );
    let threads = repo.odb().hot_path_metrics().blob_read_thread_count();
    assert!(
        threads > 1,
        "racy content checks must run on the worker pool, read on {threads} thread(s)"
    );

    let (unstaged, second_reads) = status_counting_reads(&repo, 4);
    assert!(unstaged.is_empty(), "{unstaged:?}");
    assert_eq!(second_reads, 0, "second status must trust stat data");
    assert_eq!(git_status_porcelain(root), "");
    git_in(root, &["fsck", "--no-progress"]);
}

/// A same-size edit within the racy second keeps matching stat data; only its content shows it.
fn assert_racy_same_size_edit_stays_modified(files: usize, threads: usize) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, files, true);
    git_in(root, &["config", "core.trustctime", "false"]);
    git_in(root, &["config", "index.threads", &threads.to_string()]);
    let rel = "d1/s1/f0001.txt";
    let abs = root.join(rel);
    let mut bytes = fs::read(&abs).expect("read");
    bytes[0] = b'X';
    fs::write(&abs, &bytes).expect("write");
    make_index_racy(root, files);

    let repo = open(root);
    for run in 0..2 {
        let (unstaged, _) = status_counting_reads(&repo, threads);
        assert_eq!(
            unstaged,
            vec![rel.to_owned()],
            "run {run}, threads {threads}"
        );
    }
    assert_eq!(git_status_porcelain(root), format!(" M {rel}\n"));
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn racy_same_size_edit_stays_modified_after_status_rewrites_index() {
    assert_racy_same_size_edit_stays_modified(16, 1);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn parallel_racy_same_size_edit_stays_modified_after_status_rewrites_index() {
    assert_racy_same_size_edit_stays_modified(PARALLEL_STAT_MIN_ENTRIES + 24, 4);
}

/// The standalone stat refresh (used before index writes by reset and stash) verifies racy
/// entries on the worker pool, adopts nothing for an edited one, and smudges it for later scans.
#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn stat_refresh_verifies_racy_entries_in_parallel_and_smudges_the_edited_one() {
    const FILES: usize = PARALLEL_STAT_MIN_ENTRIES + 24;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, FILES, true);
    git_in(root, &["config", "core.trustctime", "false"]);
    git_in(root, &["config", "index.threads", "4"]);
    let rel = "d2/s2/f0002.txt";
    let abs = root.join(rel);
    let mut bytes = fs::read(&abs).expect("read");
    bytes[0] = b'Y';
    fs::write(&abs, &bytes).expect("write");
    make_index_racy(root, FILES);

    let repo = open(root);
    let mut index = repo.load_index().expect("index");
    let before = index.entries.clone();
    let index_mtime = index.source_mtime;
    let metrics = repo.odb().hot_path_metrics();
    metrics.reset_blob_content_reads();
    metrics.set_blob_counting(true);
    let changed = crate::diff::refresh_index_stat_content_verified(
        repo.odb(),
        repo.git_dir(),
        &mut index,
        root,
        index_mtime,
        None,
        Some(4),
    )
    .expect("refresh");
    metrics.set_blob_counting(false);
    assert!(changed, "the edited racy entry must be invalidated");
    assert!(
        metrics.blob_read_thread_count() > 1,
        "refresh must hash in parallel"
    );
    for (old, new) in before.iter().zip(&index.entries) {
        if old.path == rel.as_bytes() {
            assert_eq!(new.size, 0, "edited racy entry is smudged");
        } else {
            assert!(
                crate::index::index_entries_stat_equivalent(
                    old,
                    new,
                    crate::index::StatMatchPolicy::default()
                ),
                "clean entry {} keeps its stat",
                String::from_utf8_lossy(&old.path)
            );
        }
    }
}

/// Git's racy-write smudge leaves `size == 0` on a non-empty blob; an empty worktree file with
/// matching timestamps must still be hashed and reported modified.
fn assert_smudged_empty_file_is_modified(files: usize, threads: usize) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_committed_repo(root, files, true);
    git_in(root, &["config", "core.trustctime", "false"]);
    let rel = "d1/s1/f0001.txt";
    let abs = root.join(rel);
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&abs)
        .expect("truncate in place");
    set_file_mtime(&abs, FileTime::from_unix_time(OLD_MTIME, 0)).expect("mtime");

    let repo = open(root);
    let mut index = repo.load_index().expect("index");
    let entry = index
        .entries
        .iter_mut()
        .find(|e| e.path == rel.as_bytes())
        .expect("entry");
    assert!(!entry.oid.is_empty_blob());
    entry.size = 0;
    repo.write_index(&mut index).expect("write smudged index");

    for run in 0..2 {
        let (unstaged, _) = status_counting_reads(&repo, threads);
        assert_eq!(
            unstaged,
            vec![rel.to_owned()],
            "run {run}, threads {threads}"
        );
    }
    assert_eq!(git_status_porcelain(root), format!(" M {rel}\n"));
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn serial_status_hashes_smudged_entry_whose_file_is_empty() {
    assert_smudged_empty_file_is_modified(24, 1);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn parallel_status_hashes_smudged_entry_whose_file_is_empty() {
    assert_smudged_empty_file_is_modified(PARALLEL_STAT_MIN_ENTRIES + 24, 4);
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
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
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
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

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
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

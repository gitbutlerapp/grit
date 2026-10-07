//! Parallel index stat preload (`core.preloadIndex`) matches serial scans.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::PARALLEL_STAT_MIN_ENTRIES;

fn assert_git_diff_files_clean(repo_root: &Path) {
    let root = repo_root.to_str().expect("utf-8 path");
    let _ = Command::new("git")
        .args(["-C", root, "update-index", "--refresh"])
        .status()
        .expect("spawn git update-index --refresh");
    let status = Command::new("git")
        .args(["-C", root, "diff-files", "--quiet"])
        .status()
        .expect("spawn git diff-files");
    if !status.success() {
        let detail = Command::new("git")
            .args(["-C", root, "diff-files"])
            .output()
            .expect("spawn git diff-files");
        panic!(
            "git diff-files --quiet must succeed after grit index refresh:\n{}",
            String::from_utf8_lossy(&detail.stdout)
        );
    }
}

fn diff_paths(model: &grit_lib::porcelain::status::StatusModel) -> (Vec<String>, Vec<String>) {
    let mut staged: Vec<_> = model.staged.iter().map(|e| e.path().to_owned()).collect();
    let mut unstaged: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    staged.sort();
    unstaged.sort();
    (staged, unstaged)
}

fn append_repo_config(repo: &Repository, section: &str, key: &str, value: &str) {
    let path = repo.git_dir.join("config");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("[{section}]\n\t{key} = {value}\n"));
    fs::write(&path, text).expect("write config");
    repo.reload_config().expect("reload config");
}

fn seed_repo(root: &Path, preload_index: bool) -> Repository {
    let repo = init_repository(root, false, "main", None, "files").expect("init");
    if !preload_index {
        append_repo_config(&repo, "core", "preloadIndex", "false");
    }
    fs::create_dir_all(root.join("mix")).expect("mkdir");
    for i in 0..5000_usize {
        let rel = format!("mix/{i:05}.txt");
        fs::write(root.join(&rel), format!("payload {i}\n")).expect("write");
    }
    grit_lib::porcelain::add::stage(
        &repo,
        &grit_lib::porcelain::add::StageOptions::default(),
        &mut NullProgress,
    )
    .expect("stage");
    let req = grit_lib::porcelain::commit::CommitRequest {
        message: "seed".into(),
        author: "T <t@e.com> 1 +0000".into(),
        committer: "T <t@e.com> 1 +0000".into(),
        allow_empty: false,
    };
    grit_lib::porcelain::commit::create_commit(&repo, &req, &mut NullProgress).expect("commit");

    // Content matches HEAD; mtimes differ so stat refresh runs (including racy re-verify).
    let now = filetime::FileTime::now();
    for rel in ["mix/00030.txt", "mix/00040.txt", "mix/00050.txt"] {
        filetime::set_file_mtime(root.join(rel), now).expect("touch");
    }
    repo
}

fn run_status(
    repo: &Repository,
    threads: usize,
) -> (grit_lib::porcelain::status::StatusModel, Vec<u8>) {
    let mut opts = StatusOptions::default();
    opts.untracked = UntrackedMode::No;
    opts.ahead_behind = false;
    opts.stat_parallel_threads = Some(threads);
    let model = status(repo, &opts, &mut NullProgress).expect("status");
    let index_bytes = fs::read(repo.index_path()).expect("index bytes");
    (model, index_bytes)
}

#[test]
fn parallel_refresh_matches_serial() {
    assert!(PARALLEL_STAT_MIN_ENTRIES <= 5000);
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(tmp.path(), true);
    let baseline_index = fs::read(repo.index_path()).expect("baseline index");

    let (serial_model, serial_index) = {
        fs::write(repo.index_path(), &baseline_index).expect("restore index");
        run_status(&repo, 1)
    };
    let (parallel_model, parallel_index) = {
        fs::write(repo.index_path(), &baseline_index).expect("restore index");
        run_status(&repo, 4)
    };

    assert_eq!(diff_paths(&serial_model), diff_paths(&parallel_model));
    assert_eq!(serial_index, parallel_index);
}

/// Grit-refreshed index entries should match the worktree the way Git's `diff-files` expects.
#[test]
fn refreshed_index_matches_git_worktree() {
    assert!(PARALLEL_STAT_MIN_ENTRIES <= 5000);
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(tmp.path(), true);
    let _model = run_status(&repo, 4).0;
    assert_git_diff_files_clean(tmp.path());
}

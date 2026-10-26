//! Shared helpers for switch overwrite Git oracle tests.
//!
//! Several test binaries include this module and each uses only part of it.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Paths system `git switch` refuses to overwrite, sorted and deduplicated.
pub fn git_switch_refused_paths(repo: &Path, branch: &str) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["switch", branch])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git switch");
    assert!(
        !out.status.success(),
        "git switch should refuse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let mut paths: Vec<String> = parse_git_overwrite_paths(&text).into_iter().collect();
    paths.sort();
    paths.dedup();
    paths
}

fn parse_git_overwrite_paths(text: &str) -> std::collections::BTreeSet<String> {
    let mut paths = std::collections::BTreeSet::new();
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("would be overwritten") {
            in_list = true;
            continue;
        }
        if in_list {
            if trimmed.is_empty()
                || trimmed.starts_with("Please ")
                || trimmed.starts_with("Aborting")
            {
                break;
            }
            paths.insert(trimmed.to_owned());
        }
    }
    paths
}

/// Repository-relative worktree file contents (excluding `.git`), sorted by path.
pub fn worktree_file_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    walk_worktree_files(root, root, &mut out);
    out
}

fn walk_worktree_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dirent");
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let meta = entry.metadata().expect("metadata");
        if meta.is_dir() {
            walk_worktree_files(root, &path, out);
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path).expect("read file");
            out.insert(rel, bytes);
        }
    }
}

/// All reflog file bytes under `.git/logs`, keyed by path relative to `.git`.
pub fn git_reflogs_snapshot(git_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let logs = git_dir.join("logs");
    let mut out = BTreeMap::new();
    if logs.is_dir() {
        walk_reflog_files(&logs, git_dir, &mut out);
    }
    out
}

fn walk_reflog_files(dir: &Path, git_dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).expect("read logs") {
        let entry = entry.expect("dirent");
        let path = entry.path();
        if path.is_dir() {
            walk_reflog_files(&path, git_dir, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(git_dir)
                .expect("under git dir")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, std::fs::read(&path).expect("read reflog"));
        }
    }
}

pub fn dir_bytes_snapshot(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap_or_default()
}

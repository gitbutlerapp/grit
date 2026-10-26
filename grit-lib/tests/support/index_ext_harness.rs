//! Helpers for split-index and untracked-cache git compatibility tests.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Read one `read_directory` trace2 counter (`node-creation`, `opendir`, …).
pub fn trace2_read_directory_counter(trace_path: &Path, key: &str) -> Option<u64> {
    let data = fs::read_to_string(trace_path).ok()?;
    for line in data.lines() {
        if !line.contains("\"event\":\"data\"") || !line.contains("\"category\":\"read_directory\"")
        {
            continue;
        }
        if !line.contains(&format!("\"key\":\"{key}\"")) {
            continue;
        }
        let value = line.split("\"value\":\"").nth(1)?.split('"').next()?;
        return value.parse().ok();
    }
    None
}

/// Run `git status --porcelain` in normal untracked mode with trace2 logging.
pub fn git_status_porcelain_normal_with_trace(repo_dir: &Path, trace_path: &Path) {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .env("GIT_TRACE2_EVENT", trace_path)
        .env("GIT_TRACE2_EVENT_NESTING", "10")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .args(["-c", "core.untrackedCache=true", "status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn git_status_porcelain_normal(repo_dir: &Path) -> String {
    git(
        repo_dir,
        &["-c", "core.untrackedCache=true", "status", "--porcelain"],
    )
}

use grit_lib::index::IndexEntry;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use super::history_harness::{git, git_ls_files_stage};

pub fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

pub fn append_git_config(git_dir: &Path, extra: &str) {
    let path = git_dir.join("config");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(extra);
    fs::write(path, text).expect("write config");
}

pub fn fsck_strict(repo_dir: &Path) {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn grit_index_stage_rows(repo: &Repository) -> Vec<(String, u32, String, ObjectId)> {
    let idx = repo.index_handle().load().expect("load index");
    let mut rows: Vec<(String, u32, String, ObjectId)> = idx
        .entries()
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                u32::from(e.stage()),
                format!("{:o}", e.mode),
                e.oid,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    rows
}

pub fn assert_grit_index_matches_git_ls_files(repo_dir: &Path, repo: &Repository) {
    assert_eq!(
        git_ls_files_stage(repo_dir),
        grit_index_stage_rows(repo),
        "grit merged index must match git ls-files -s"
    );
}

pub fn index_contains_extension(index_bytes: &[u8], sig: &[u8; 4]) -> bool {
    index_bytes.windows(4).any(|w| w == sig)
}

pub fn extension_payload(index_bytes: &[u8], sig: &[u8; 4]) -> Option<Vec<u8>> {
    let pos = index_bytes.windows(4).position(|w| w == sig)?;
    if pos + 8 > index_bytes.len() {
        return None;
    }
    let ext_sz = u32::from_be_bytes(index_bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
    let start = pos + 8;
    let end = start.checked_add(ext_sz)?;
    if end > index_bytes.len() {
        return None;
    }
    Some(index_bytes[start..end].to_vec())
}

pub fn list_sharedindex_paths(git_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = fs::read_dir(git_dir) else {
        return out;
    };
    for ent in read.flatten() {
        let name = ent.file_name();
        if name.to_str().is_some_and(|s| s.starts_with("sharedindex.")) {
            out.push(ent.path());
        }
    }
    out.sort();
    out
}

pub fn populate_flat_files(root: &Path, prefix: &str, count: usize) {
    for i in 0..count {
        fs::write(
            root.join(format!("{prefix}{i:04}.txt")),
            format!("body {i}\n"),
        )
        .expect("write file");
    }
}

pub fn git_update_index_split(repo_dir: &Path) {
    git(repo_dir, &["update-index", "--split-index"]);
}

pub fn git_update_index_untracked_cache(repo_dir: &Path) {
    git(repo_dir, &["update-index", "--untracked-cache"]);
}

pub fn git_porcelain_untracked(repo_dir: &Path) -> Vec<String> {
    git(
        repo_dir,
        &[
            "-c",
            "core.untrackedCache=true",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ],
    )
    .lines()
    .filter(|l| l.starts_with("?? "))
    .map(|l| l[3..].to_string())
    .collect()
}

pub fn entry_from_worktree_path(repo: &Repository, path: &str, payload: &[u8]) -> IndexEntry {
    let oid = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, payload)
        .expect("write blob");
    IndexEntry {
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        dev: 1,
        ino: 1,
        mode: grit_lib::index::MODE_REGULAR,
        uid: 1000,
        gid: 1000,
        size: payload.len() as u32,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

pub fn read_index_bytes(repo: &Repository) -> Vec<u8> {
    fs::read(repo.index_handle().path()).expect("read index")
}

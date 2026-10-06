//! Integration tests for `git repo info` (reference storage and key listing).
//!
//! Upstream Git still exposes `references.format` on master/next; when the
//! rename to `references.storageFormat` lands in git.git, update
//! `REF_STORAGE_KEY` here and in `grit-git/src/commands/repo.rs` together.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT_BIN: &str = env!("CARGO_BIN_EXE_grit-git");

/// Info key for reference storage backend (matches upstream `repo_info_field`).
const REF_STORAGE_KEY: &str = "references.format";

struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("grit-repo-info-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap_or_else(|e| panic!("create temp dir {}: {e}", p.display()));
    p
}

fn grit(args: &[&str], dir: &Path) -> Output {
    let out = Command::new(GRIT_BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn grit-git {args:?}: {e}"));
    Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn init_repo(parent: &Path, name: &str, ref_format: &str) -> PathBuf {
    let repo = parent.join(name);
    let init = grit(&["init", "--ref-format", ref_format, name], parent);
    assert!(init.ok(), "init --ref-format={ref_format}: {}", init.stderr);
    repo
}

#[test]
fn keys_list_includes_reference_storage_key() {
    let tmp = unique_tmp("keys");
    let _repo = init_repo(&tmp, "repo", "files");
    let out = grit(&["repo", "info", "--keys"], &_repo);
    assert!(out.ok(), "{}", out.stderr);
    let keys: Vec<&str> = out.stdout.lines().collect();
    assert!(keys.contains(&REF_STORAGE_KEY));
    assert!(keys.contains(&"layout.bare"));
    assert!(keys.contains(&"object.format"));
}

#[test]
fn reference_storage_files_and_reftable() {
    let tmp = unique_tmp("reffmt");
    let files_repo = init_repo(&tmp, "files", "files");
    let out = grit(&["repo", "info", REF_STORAGE_KEY], &files_repo);
    assert!(out.ok(), "{}", out.stderr);
    assert_eq!(out.stdout.trim(), format!("{REF_STORAGE_KEY}=files"));

    let reftable_repo = init_repo(&tmp, "reftable", "reftable");
    let out = grit(&["repo", "info", REF_STORAGE_KEY], &reftable_repo);
    assert!(out.ok(), "{}", out.stderr);
    assert_eq!(out.stdout.trim(), format!("{REF_STORAGE_KEY}=reftable"));
}

#[test]
fn all_matches_keys_expansion() {
    let tmp = unique_tmp("all");
    let repo = init_repo(&tmp, "repo", "files");
    let keys_out = grit(&["repo", "info", "--keys"], &repo);
    assert!(keys_out.ok(), "{}", keys_out.stderr);
    let keys: Vec<String> = keys_out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();

    let mut args = vec!["repo", "info"];
    for key in &keys {
        args.push(key);
    }
    let by_keys = grit(&args, &repo);
    assert!(by_keys.ok(), "{}", by_keys.stderr);

    let all = grit(&["repo", "info", "--all"], &repo);
    assert!(all.ok(), "{}", all.stderr);
    assert_eq!(by_keys.stdout, all.stdout);
}

#[test]
fn unknown_key_errors_and_valid_key_still_prints() {
    let tmp = unique_tmp("invalid");
    let repo = init_repo(&tmp, "repo", "files");
    let out = grit(&["repo", "info", "foo", REF_STORAGE_KEY, "bar"], &repo);
    assert_eq!(out.status, Some(1));
    assert_eq!(out.stdout.trim(), format!("{REF_STORAGE_KEY}=files"));
    assert!(
        out.stderr.contains("error: key 'foo' not found"),
        "stderr: {}",
        out.stderr
    );
}

/// After upstream renames the key to `references.storageFormat`, this key must
/// no longer be accepted (hard rename, no alias).
#[test]
fn legacy_storage_format_key_name_rejected_when_renamed() {
    if REF_STORAGE_KEY == "references.format" {
        // Current upstream: old name is still the canonical key.
        let tmp = unique_tmp("legacy");
        let repo = init_repo(&tmp, "repo", "files");
        let out = grit(&["repo", "info", "references.storageFormat"], &repo);
        assert_eq!(out.status, Some(1));
        assert!(out.stderr.contains("references.storageFormat"));
        return;
    }

    let tmp = unique_tmp("legacy");
    let repo = init_repo(&tmp, "repo", "files");
    let out = grit(&["repo", "info", "references.format"], &repo);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("references.format"));
}

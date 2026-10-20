//! `diff_paths` compatibility with `git diff --no-index` (upstream t4053 semantics).

use std::path::Path;
use std::process::Command;

use grit_lib::diff::{diff_paths, DiffOptions, NoIndexError, RenameDetection};
use tempfile::TempDir;

fn git_name_status(old: &Path, new: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--name-status",
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect()
}

fn grit_name_status(old: &Path, new: &Path) -> Vec<String> {
    let mut opts = DiffOptions::default();
    opts.renames = RenameDetection::Off;
    let files = diff_paths(old, new, &opts).expect("diff_paths");
    files
        .iter()
        .map(|f| format!("{}\t{}", f.status.letter(), f.path))
        .collect()
}

#[test]
fn no_index_file_vs_file_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"one\n").expect("write");
    std::fs::write(&b, b"two\n").expect("write");
    assert_eq!(grit_name_status(&a, &b), git_name_status(&a, &b));
    let mut opts = DiffOptions::default();
    opts.renames = RenameDetection::Off;
    let files = diff_paths(&a, &b, &opts).expect("diff");
    assert_eq!(files.len(), 1);
    assert!(!files[0].hunks.is_empty());
}

#[test]
fn no_index_dir_vs_dir_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a).expect("mkdir");
    std::fs::create_dir(&b).expect("mkdir");
    std::fs::write(a.join("1"), b"1\n").expect("write");
    std::fs::write(a.join("2"), b"old\n").expect("write");
    std::fs::write(b.join("1"), b"1\n").expect("write");
    std::fs::write(b.join("2"), b"new\n").expect("write");
    assert_eq!(grit_name_status(&a, &b), git_name_status(&a, &b));
}

#[test]
fn no_index_file_vs_dir_uses_basename() {
    let dir = TempDir::new().expect("tempdir");
    let d = dir.path().join("d");
    let f = dir.path().join("f.txt");
    std::fs::create_dir(&d).expect("mkdir");
    std::fs::write(d.join("f.txt"), b"1\n").expect("write");
    std::fs::write(&f, b"2\n").expect("write");
    assert_eq!(grit_name_status(&d, &f), git_name_status(&d, &f));
}

#[test]
fn no_index_missing_path_is_typed_error() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("missing");
    let b = dir.path().join("b.txt");
    std::fs::write(&b, b"x\n").expect("write");
    let err = diff_paths(&a, &b, &DiffOptions::default()).unwrap_err();
    assert!(matches!(err, NoIndexError::PathNotAccessible { .. }));
}

#[cfg(unix)]
#[test]
fn no_index_symlink_and_mode() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a).expect("mkdir");
    std::fs::create_dir(&b).expect("mkdir");
    std::fs::write(a.join("target"), b"data\n").expect("write");
    symlink("target", a.join("link")).expect("symlink");
    std::fs::write(b.join("target"), b"data\n").expect("write");
    symlink("target", b.join("link")).expect("symlink");
    let mut opts = DiffOptions::default();
    opts.renames = RenameDetection::Off;
    let files = diff_paths(&a, &b, &opts).expect("diff");
    assert_eq!(files.len(), 0);
    std::fs::write(a.join("run.sh"), b"#!/bin/sh\n").expect("write");
    let mut perms = std::fs::metadata(a.join("run.sh"))
        .expect("meta")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(a.join("run.sh"), perms).expect("chmod");
    std::fs::write(b.join("run.sh"), b"#!/bin/sh\n").expect("write");
    let files = diff_paths(&a, &b, &opts).expect("diff");
    let mode_only = files
        .iter()
        .find(|f| f.path.ends_with("run.sh"))
        .expect("run.sh diff");
    assert_eq!(mode_only.old_mode.as_deref(), Some("100755"));
    assert_eq!(mode_only.new_mode.as_deref(), Some("100644"));
}

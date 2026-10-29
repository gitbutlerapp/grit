//! `diff_paths` compatibility with `git diff --no-index` (upstream t4053 semantics).

use std::path::Path;
use std::process::Command;

use grit_lib::diff::{
    diff_paths, write_paths_patch, DiffOptions, DiffStatus, FileDiff, NoIndexError, RenameDetection,
};
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

/// Git `--name-status` path column (left-hand path for modifications).
fn grit_name_status_line(f: &FileDiff) -> String {
    let path_for_git = match f.status {
        DiffStatus::Added => f.path.clone(),
        DiffStatus::Deleted => f.old_path.clone().unwrap_or(f.path.clone()),
        _ => f.old_path.clone().unwrap_or(f.path.clone()),
    };
    format!("{}\t{}", f.status.letter(), path_for_git)
}

fn grit_name_status(old: &Path, new: &Path) -> Vec<String> {
    let mut opts = DiffOptions::default();
    opts.renames = RenameDetection::Off;
    let files = diff_paths(old, new, &opts).expect("diff_paths");
    files.iter().map(grit_name_status_line).collect()
}

#[test]
fn no_index_stat_only_populates_line_stat_without_hunks() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"one\n").expect("write");
    std::fs::write(&b, b"two\nextra\n").expect("write");
    let opts = DiffOptions::default()
        .with_no_index_stat_only(true)
        .with_renames(RenameDetection::Off);
    let files = diff_paths(&a, &b, &opts).expect("diff_paths");
    assert_eq!(files.len(), 1);
    assert!(files[0].hunks.is_empty());
    let stat = files[0].line_stat.expect("line_stat");
    assert_eq!(stat.insertions, 2);
    assert_eq!(stat.deletions, 1);

    let out = Command::new("git")
        .args(["diff", "--no-index", "--numstat"])
        .arg(&a)
        .arg(&b)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git numstat");
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.lines().next().unwrap_or("");
    let mut parts = line.split('\t');
    let git_ins: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let git_del: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    assert_eq!(stat.insertions, git_ins);
    assert_eq!(stat.deletions, git_del);
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
    assert_eq!(files[0].path, b.to_string_lossy());
    assert_eq!(files[0].old_path.as_deref(), Some(a.to_str().unwrap()));
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
fn no_index_dir_unique_child_add_delete() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old");
    let new = dir.path().join("new");
    std::fs::create_dir(&old).expect("mkdir");
    std::fs::create_dir(&new).expect("mkdir");
    std::fs::write(old.join("old"), b"x\n").expect("write");
    std::fs::write(new.join("new"), b"y\n").expect("write");
    assert_eq!(grit_name_status(&old, &new), git_name_status(&old, &new));
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
fn no_index_dir_file_type_collision_one_add_one_delete() {
    let dir = TempDir::new().expect("tempdir");
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    std::fs::create_dir_all(left.join("sub")).expect("mkdir");
    std::fs::write(left.join("sub").join("x"), b"in dir\n").expect("write");
    std::fs::create_dir(&right).expect("mkdir");
    std::fs::write(right.join("sub"), b"file\n").expect("write");
    let git_lines = git_name_status(&left, &right);
    let grit_lines = grit_name_status(&left, &right);
    assert_eq!(grit_lines, git_lines);
    assert!(git_lines.iter().any(|l| l.starts_with('A')));
    assert!(git_lines.iter().any(|l| l.starts_with('D')));
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

#[test]
fn no_index_rename_detection_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a).expect("mkdir");
    std::fs::create_dir(&b).expect("mkdir");
    std::fs::write(a.join("f"), b"same\n").expect("write");
    std::fs::write(b.join("g"), b"same\n").expect("write");
    let git_lines = git_name_status(&a, &b);
    assert_eq!(git_lines.len(), 1);
    assert!(git_lines[0].starts_with('R'));
    let files = diff_paths(&a, &b, &DiffOptions::default()).expect("diff");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].status, DiffStatus::Renamed);
    assert!(files[0].path.ends_with("/b/g") || files[0].path.ends_with("b/g"));
    assert!(files[0]
        .old_path
        .as_deref()
        .is_some_and(|p| p.ends_with("/a/f") || p.ends_with("a/f")));
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
    assert_eq!(mode_only.status, DiffStatus::Modified);
    assert_eq!(mode_only.old_mode.as_deref(), Some("100755"));
    assert_eq!(mode_only.new_mode.as_deref(), Some("100644"));
}

#[cfg(unix)]
#[test]
fn no_index_file_vs_root_matches_git() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert!(manifest.is_file(), "workspace Cargo.toml");
    let git = Command::new("git")
        .args(["diff", "--no-index", manifest.to_str().unwrap(), "/"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert_ne!(git.status.code(), Some(0), "git should fail on /Cargo.toml");
    let stderr = String::from_utf8_lossy(&git.stderr);
    assert!(
        stderr.contains("Could not access") || stderr.contains("No such file"),
        "unexpected git stderr: {stderr}"
    );

    let err = diff_paths(&manifest, Path::new("/"), &DiffOptions::default()).unwrap_err();
    assert!(matches!(err, NoIndexError::PathNotAccessible { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("/Cargo.toml") || msg.contains("Cargo.toml"),
        "expected joined root path in error, got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn no_index_append_basename_trailing_slash() {
    let dir = TempDir::new().expect("tempdir");
    let d = dir.path().join("d");
    std::fs::create_dir(&d).expect("mkdir");
    std::fs::write(d.join("f.txt"), b"1\n").expect("write");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"2\n").expect("write");
    let d_slash = format!("{}/", d.display());
    assert_eq!(
        grit_name_status(Path::new(&d_slash), &f),
        git_name_status(Path::new(&d_slash), &f)
    );
}

#[test]
fn write_paths_patch_emits_unified_patch_for_differing_files() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"left\n").expect("write");
    std::fs::write(&b, b"right\n").expect("write");

    let mut patch = Vec::new();
    let changed =
        write_paths_patch(&a, &b, &DiffOptions::default(), &mut patch).expect("write_paths_patch");
    assert!(changed);
    let text = String::from_utf8(patch).expect("utf8 patch");
    assert!(text.contains("@@"), "expected unified hunk header:\n{text}");
    assert!(
        text.contains("-left") && text.contains("+right"),
        "expected line change in patch:\n{text}"
    );

    std::fs::write(&a, b"same\n").expect("write");
    std::fs::write(&b, b"same\n").expect("write");
    let mut empty = Vec::new();
    let unchanged =
        write_paths_patch(&a, &b, &DiffOptions::default(), &mut empty).expect("write_paths_patch");
    assert!(!unchanged);
    assert!(empty.is_empty());
}

//! `--pathspec-from-file` matches Git for add, rm, and restore.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let null = null_config();
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("run grit")
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let null = null_config();
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("run git")
}

fn ls_files_sorted(dir: &std::path::Path) -> Vec<String> {
    let out = run_git(dir, &["ls-files", "-s"]);
    assert!(
        out.status.success(),
        "ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

fn init_repo(grit: &str, dir: &tempfile::TempDir) {
    assert!(run_grit(grit, dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n").expect("write a");
    fs::write(dir.path().join("b.txt"), b"b\n").expect("write b");
    fs::write(dir.path().join("c.txt"), b"c\n").expect("write c");
    assert!(run_grit(grit, dir.path(), &["add", "."]).status.success());
    let null = null_config();
    assert!(Command::new(grit)
        .current_dir(dir.path())
        .args(["commit", "-m", "initial"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("commit")
        .success());
    fs::write(dir.path().join("a.txt"), b"a2\n").expect("modify a");
    fs::write(dir.path().join("b.txt"), b"b2\n").expect("modify b");
}

#[test]
fn add_pathspec_from_file_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);

    let list = dir.path().join("paths.txt");
    fs::write(&list, b"a.txt\nb.txt\n").expect("write list");

    assert!(run_git(
        dir.path(),
        &["add", "--pathspec-from-file", list.to_str().unwrap()]
    )
    .status
    .success());
    let git_index = ls_files_sorted(dir.path());

    let dir2 = tempfile::TempDir::new().expect("tempdir2");
    init_repo(grit, &dir2);
    let list2 = dir2.path().join("paths.txt");
    fs::write(&list2, b"a.txt\nb.txt\n").expect("write list");
    assert!(run_grit(
        grit,
        dir2.path(),
        &["add", "--pathspec-from-file", list2.to_str().unwrap()]
    )
    .status
    .success());
    let grit_index = ls_files_sorted(dir2.path());
    assert_eq!(git_index, grit_index);
}

#[test]
fn add_pathspec_from_stdin_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);

    let null = null_config();
    let mut git = Command::new("git")
        .current_dir(dir.path())
        .args(["add", "--pathspec-from-file", "-"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    git.stdin
        .take()
        .expect("stdin")
        .write_all(b"a.txt\n")
        .expect("write stdin");
    assert!(git.wait().expect("wait").success());

    let git_index = ls_files_sorted(dir.path());

    let dir2 = tempfile::TempDir::new().expect("tempdir2");
    init_repo(grit, &dir2);
    let mut grit_cmd = Command::new(grit)
        .current_dir(dir2.path())
        .args(["add", "--pathspec-from-file", "-"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit");
    grit_cmd
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"a.txt\n")
        .expect("write stdin");
    assert!(grit_cmd.wait().expect("wait").success());

    let grit_index = ls_files_sorted(dir2.path());
    assert_eq!(git_index, grit_index);
}

#[test]
fn rm_pathspec_from_file_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);

    let list = dir.path().join("rm.txt");
    fs::write(&list, b"b.txt\n").expect("write list");
    assert!(run_git(
        dir.path(),
        &["rm", "-f", "--pathspec-from-file", list.to_str().unwrap()]
    )
    .status
    .success());
    let git_index = ls_files_sorted(dir.path());

    let dir2 = tempfile::TempDir::new().expect("tempdir2");
    init_repo(grit, &dir2);
    let list2 = dir2.path().join("rm.txt");
    fs::write(&list2, b"b.txt\n").expect("write list");
    assert!(run_grit(
        grit,
        dir2.path(),
        &["rm", "-f", "--pathspec-from-file", list2.to_str().unwrap()]
    )
    .status
    .success());
    let grit_index = ls_files_sorted(dir2.path());
    assert_eq!(git_index, grit_index);
}

#[test]
fn restore_pathspec_from_file_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);

    let list = dir.path().join("restore.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    assert!(run_git(
        dir.path(),
        &["restore", "--pathspec-from-file", list.to_str().unwrap()]
    )
    .status
    .success());
    let git_worktree = fs::read_to_string(dir.path().join("a.txt")).expect("read a");

    let dir2 = tempfile::TempDir::new().expect("tempdir2");
    init_repo(grit, &dir2);
    let list2 = dir2.path().join("restore.txt");
    fs::write(&list2, b"a.txt\n").expect("write list");
    assert!(run_grit(
        grit,
        dir2.path(),
        &["restore", "--pathspec-from-file", list2.to_str().unwrap()]
    )
    .status
    .success());
    let grit_worktree = fs::read_to_string(dir2.path().join("a.txt")).expect("read a");
    assert_eq!(git_worktree, grit_worktree);
    assert_eq!(git_worktree, "a\n");
}

#[test]
fn add_blank_line_pathspec_from_file_fails_like_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);
    fs::write(dir.path().join("extra.txt"), b"x\n").expect("write");
    fs::write(dir.path().join("list.txt"), b"\n").expect("blank");

    let git_out = run_git(dir.path(), &["add", "--pathspec-from-file", "list.txt"]);
    assert!(!git_out.status.success());

    let grit_out = run_grit(
        grit,
        dir.path(),
        &["add", "--pathspec-from-file", "list.txt"],
    );
    assert!(!grit_out.status.success());
    let stderr = String::from_utf8_lossy(&grit_out.stderr);
    assert!(
        stderr.contains("invalid path ''") || stderr.contains("empty"),
        "stderr: {stderr}"
    );

    let staged = ls_files_sorted(dir.path());
    assert!(
        staged.iter().all(|l| !l.contains("extra.txt")),
        "must not stage whole tree on blank pathspec line: {staged:?}"
    );
}

#[test]
fn add_nul_empty_record_fails_like_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);
    fs::write(dir.path().join("a.txt"), b"a\n").expect("write");
    fs::write(dir.path().join("b.txt"), b"b\n").expect("write");
    let list = dir.path().join("list.bin");
    fs::write(&list, b"a.txt\0\0b.txt").expect("nul empty");

    let git_out = run_git(
        dir.path(),
        &[
            "add",
            "--pathspec-from-file",
            list.file_name().unwrap().to_str().unwrap(),
            "--pathspec-file-nul",
        ],
    );
    assert!(!git_out.status.success());

    let grit_out = run_grit(
        grit,
        dir.path(),
        &[
            "add",
            "--pathspec-from-file",
            list.to_str().unwrap(),
            "--pathspec-file-nul",
        ],
    );
    assert!(!grit_out.status.success());
}

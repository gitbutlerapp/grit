//! CLI tests for `grit diff --no-index`.

use std::process::Command;

use grit_test_support::configure_git_env;
use tempfile::TempDir;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_diff_no_index(old: &str, new: &str) -> (i32, String) {
    let out = Command::new("git")
        .args(["diff", "--no-index", old, new])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    (
        out.status.code().unwrap_or(128),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn grit_diff_no_index(cwd: &std::path::Path, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    let mut full = vec!["diff", "--no-index"];
    full.extend(args);
    let out = cmd.current_dir(cwd).args(full).output().expect("grit");
    (
        out.status.code().unwrap_or(128),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn grit_diff_no_index_two(cwd: &std::path::Path, old: &str, new: &str) -> (i32, String) {
    grit_diff_no_index(cwd, &[old, new])
}

#[test]
fn diff_no_index_outside_repository() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"1\n").expect("write");
    std::fs::write(&b, b"2\n").expect("write");
    let (code, out) =
        grit_diff_no_index_two(dir.path(), &a.to_string_lossy(), &b.to_string_lossy());
    assert_eq!(code, 1);
    assert!(out.contains("a.txt") || out.contains("-") || out.contains("+"));
}

#[test]
fn diff_no_index_exit_code_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"same\n").expect("write");
    std::fs::write(&b, b"same\n").expect("write");
    let a_s = a.to_string_lossy().into_owned();
    let b_s = b.to_string_lossy().into_owned();
    let (git_code, _) = git_diff_no_index(&a_s, &b_s);
    let (grit_code, _) = grit_diff_no_index_two(dir.path(), &a_s, &b_s);
    assert_eq!(grit_code, git_code);

    std::fs::write(&b, b"different\n").expect("write");
    let (git_code, _) = git_diff_no_index(&a_s, &b_s);
    let (grit_code, _) = grit_diff_no_index_two(dir.path(), &a_s, &b_s);
    assert_eq!(grit_code, git_code);
    assert_eq!(grit_code, 1);
}

#[test]
fn diff_no_index_numstat_matches_git_on_repo_files() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let cargo = root.join("Cargo.toml");
    let readme = root.join("README.md");
    let cargo_s = cargo.to_string_lossy();
    let readme_s = readme.to_string_lossy();
    let git_out = Command::new("git")
        .current_dir(&root)
        .args([
            "diff",
            "--no-index",
            "--numstat",
            cargo_s.as_ref(),
            readme_s.as_ref(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    let (_, grit_out) =
        grit_diff_no_index(&root, &["--numstat", cargo_s.as_ref(), readme_s.as_ref()]);
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let git_line = git_stdout.lines().next().unwrap_or("");
    let grit_line = grit_out.lines().next().unwrap_or("");
    let parse_counts = |line: &str| -> (usize, usize) {
        let mut parts = line.split('\t');
        let ins: usize = parts.next().unwrap_or("0").parse().expect("insertions");
        let del: usize = parts.next().unwrap_or("0").parse().expect("deletions");
        (ins, del)
    };
    assert_eq!(
        parse_counts(grit_line),
        parse_counts(git_line),
        "numstat counts mismatch: git={git_line:?} grit={grit_line:?}"
    );
    assert!(
        parse_counts(git_line).0 > 0 && parse_counts(git_line).1 > 0,
        "expected non-zero line counts"
    );
}

#[test]
fn diff_no_index_dashed_paths_after_double_dash() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("-a.txt");
    let b = dir.path().join("-b.txt");
    std::fs::write(&a, b"1\n").expect("write");
    std::fs::write(&b, b"2\n").expect("write");
    let (code, out) = grit_diff_no_index(dir.path(), &["--", "-a.txt", "-b.txt"]);
    assert_eq!(code, 1);
    assert!(out.contains("-a.txt") || out.contains('+'));
}

#[test]
fn diff_no_index_missing_path_exits_two() {
    let dir = TempDir::new().expect("tempdir");
    let (code, _) = grit_diff_no_index(dir.path(), &["/no/such/path-a", "/no/such/path-b"]);
    assert_eq!(
        code, 2,
        "errors must not use exit 1 (same as 'files differ')"
    );
}

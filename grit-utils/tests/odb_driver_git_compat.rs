//! Driver output must match system `git` on a small repacked fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bench_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"))
}

fn system_git() -> PathBuf {
    PathBuf::from("git")
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
}

fn git_cmd(git: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(git);
    cmd.current_dir(dir);
    git_env(&mut cmd);
    cmd
}

fn init_small_repo(git: &Path, dir: &Path) {
    assert!(git_cmd(git, dir)
        .args(["init", "-q", "-b", "main"])
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    assert!(git_cmd(git, dir)
        .args(["add", "a.txt"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["commit", "-q", "-m", "first"])
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.join("a.txt"), "two\n").unwrap();
    assert!(git_cmd(git, dir)
        .args(["add", "a.txt"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["commit", "-q", "-m", "second"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["repack", "-a", "-d", "-f"])
        .status()
        .unwrap()
        .success());
}

fn init_merge_head_repo(git: &Path, dir: &Path) {
    init_small_repo(git, dir);
    assert!(git_cmd(git, dir)
        .args(["checkout", "-q", "-b", "side"])
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.join("a.txt"), "side\n").unwrap();
    assert!(git_cmd(git, dir)
        .args(["add", "a.txt"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["commit", "-q", "-m", "on side"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["checkout", "-q", "main"])
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.join("b.txt"), "main\n").unwrap();
    assert!(git_cmd(git, dir)
        .args(["add", "b.txt"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["commit", "-q", "-m", "on main"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["merge", "-q", "--no-edit", "side"])
        .status()
        .unwrap()
        .success());
    assert!(git_cmd(git, dir)
        .args(["repack", "-a", "-d", "-f"])
        .status()
        .unwrap()
        .success());
}

fn git_out(git: &Path, dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = git_cmd(git, dir).args(args).output().expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn drive_out(sub: &str, dir: &Path, extra: &[&str]) -> Vec<u8> {
    let dir_s = dir.to_string_lossy();
    let mut args = vec!["drive", sub, dir_s.as_ref()];
    args.extend_from_slice(extra);
    let out = Command::new(bench_exe())
        .args(&args)
        .output()
        .expect("grit-bench drive");
    assert!(
        out.status.success(),
        "{}: {}",
        sub,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn odb_driver_cat_file_matches_git() {
    let git = system_git();
    let dir = TempDir::new().unwrap();
    init_small_repo(&git, dir.path());
    let git_batch = git_out(
        &git,
        dir.path(),
        &["cat-file", "--batch", "--batch-all-objects", "--unordered"],
    );
    let grit_batch = drive_out("cat-file-batch-all-unordered", dir.path(), &[]);
    assert_eq!(grit_batch, git_batch);

    let oids = git_out(
        &git,
        dir.path(),
        &[
            "cat-file",
            "--batch-all-objects",
            "--unordered",
            "--batch-check=%(objectname)",
        ],
    );
    let oid_text = String::from_utf8_lossy(&oids);
    let mut sorted: Vec<&str> = oid_text.lines().collect();
    sorted.sort();
    let oid_file = dir.path().join("oids.txt");
    std::fs::write(&oid_file, format!("{}\n", sorted.join("\n"))).unwrap();
    let git_sorted = git_cmd(&git, dir.path())
        .args(["cat-file", "--batch"])
        .stdin(std::fs::File::open(&oid_file).unwrap())
        .output()
        .unwrap()
        .stdout;
    let grit_sorted = Command::new(bench_exe())
        .args(["drive", "cat-file-batch", dir.path().to_str().unwrap()])
        .stdin(std::fs::File::open(&oid_file).unwrap())
        .output()
        .unwrap()
        .stdout;
    assert_eq!(grit_sorted, git_sorted);
}

#[test]
fn odb_driver_rev_list_objects_matches_git() {
    let git = system_git();
    let dir = TempDir::new().unwrap();
    init_small_repo(&git, dir.path());
    let git_out = git_out(&git, dir.path(), &["rev-list", "--objects", "--all"]);
    let grit_out = drive_out("rev-list-objects", dir.path(), &[]);
    assert_eq!(grit_out, git_out);
}

#[test]
fn odb_driver_log_patch_matches_git() {
    let git = system_git();
    let dir = TempDir::new().unwrap();
    init_small_repo(&git, dir.path());
    let git_out = git_out(&git, dir.path(), &["log", "-p", "-2"]);
    let grit_out = drive_out("log-patch", dir.path(), &["2"]);
    assert_eq!(grit_out, git_out);
}

#[test]
fn odb_driver_log_patch_merge_matches_git() {
    let git = system_git();
    let dir = TempDir::new().unwrap();
    init_merge_head_repo(&git, dir.path());
    let git_out = git_out(&git, dir.path(), &["log", "-p", "-1"]);
    let grit_out = drive_out("log-patch", dir.path(), &["1"]);
    assert_eq!(grit_out, git_out);
    let git_diff_headers = git_out
        .windows(b"diff --git".len())
        .filter(|w| *w == b"diff --git")
        .count();
    assert_eq!(git_diff_headers, 0, "merge HEAD should not emit patches");
}

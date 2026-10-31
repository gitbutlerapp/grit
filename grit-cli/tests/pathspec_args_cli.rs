//! `--pathspec-from-file` matches Git for add, rm, restore, log, and diff.

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

fn commit_all(grit: &str, dir: &std::path::Path, message: &str) {
    let null = null_config();
    assert!(run_grit(grit, dir, &["add", "."]).status.success());
    assert!(Command::new(grit)
        .current_dir(dir)
        .args(["commit", "-m", message])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("commit")
        .success());
}

fn pathspec_lines_from_file(path: &std::path::Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("read pathspec file")
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn log_oids_git(dir: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--format=%H"];
    args.extend_from_slice(extra);
    let out = run_git(dir, &args);
    assert!(
        out.status.success(),
        "git log failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn log_oids_grit(grit: &str, dir: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--json"];
    args.extend_from_slice(extra);
    let out = run_grit(grit, dir, &args);
    assert!(
        out.status.success(),
        "grit log failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_string())
        .collect()
}

#[test]
fn log_pathspec_from_file_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);
    commit_all(grit, dir.path(), "touch ab");
    fs::write(dir.path().join("c.txt"), b"c2\n").expect("modify c");
    commit_all(grit, dir.path(), "touch c only");

    let list = dir.path().join("paths.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();
    let grit_oids = log_oids_grit(
        grit,
        dir.path(),
        &["-n", "10", "--pathspec-from-file", list_arg],
    );
    let paths = pathspec_lines_from_file(&list);
    let mut git_args = vec!["-n", "10", "--"];
    git_args.extend(paths.iter().map(String::as_str));
    let git = log_oids_git(dir.path(), &git_args);
    assert_eq!(grit_oids, git);
}

#[test]
fn diff_pathspec_from_file_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);
    commit_all(grit, dir.path(), "touch ab");

    let list = dir.path().join("paths.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();

    let paths = pathspec_lines_from_file(&list);
    let mut git_args = vec!["diff", "--name-only", "HEAD~1", "--"];
    git_args.extend(paths.iter().map(String::as_str));
    let git_out = run_git(dir.path(), &git_args);
    assert!(
        git_out.status.success(),
        "git diff: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_names: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .map(str::to_string)
        .collect();

    let grit_out = run_grit(
        grit,
        dir.path(),
        &[
            "diff",
            "--name-only",
            "HEAD~1",
            "--pathspec-from-file",
            list_arg,
        ],
    );
    assert!(
        grit_out.status.success(),
        "grit diff: {}",
        String::from_utf8_lossy(&grit_out.stderr)
    );
    let grit_names: Vec<String> = String::from_utf8_lossy(&grit_out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(grit_names, git_names);
}

#[test]
fn diff_worktree_pathspec_from_stdin_filters_once() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);

    let null = null_config();
    let run_with_stdin = |cmd: &str, args: &[&str]| -> std::process::Output {
        let mut child = Command::new(cmd)
            .current_dir(dir.path())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &null)
            .env("GIT_CONFIG_SYSTEM", &null)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        {
            let stdin = child.stdin.as_mut().expect("stdin");
            stdin.write_all(b"a.txt\n").expect("write stdin");
        }
        child.wait_with_output().expect("wait")
    };

    let expected = run_git(dir.path(), &["diff", "--name-only", "--", "a.txt"]);
    assert!(expected.status.success());
    let expected_names: Vec<String> = String::from_utf8_lossy(&expected.stdout)
        .lines()
        .map(str::to_string)
        .collect();

    let grit_out = run_with_stdin(grit, &["diff", "--name-only", "--pathspec-from-file", "-"]);
    assert!(
        grit_out.status.success(),
        "grit diff stdin: {}",
        String::from_utf8_lossy(&grit_out.stderr)
    );
    let grit_names: Vec<String> = String::from_utf8_lossy(&grit_out.stdout)
        .lines()
        .map(str::to_string)
        .collect();

    assert_eq!(grit_names, expected_names);
    assert_eq!(grit_names, vec!["a.txt".to_string()]);
}

fn init_bare_two_commit_repo(grit: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_repo(grit, &dir);
    commit_all(grit, dir.path(), "second");
    let bare = dir.path().join("bare.git");
    assert!(run_git(
        dir.path(),
        &[
            "clone",
            "--bare",
            ".",
            bare.file_name().unwrap().to_str().unwrap()
        ]
    )
    .status
    .success());
    (dir, bare)
}

fn log_oids_in_bare(dir: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["--git-dir", dir.to_str().unwrap(), "log", "--format=%H"];
    args.extend_from_slice(extra);
    let out = Command::new("git")
        .args(&args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("git log bare");
    assert!(
        out.status.success(),
        "git log bare: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn run_grit_bare(grit: &str, git_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(grit)
        .current_dir(git_dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit bare")
}

#[test]
fn log_trailing_path_in_bare_repo_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let (_wt, bare) = init_bare_two_commit_repo(grit);
    let git = log_oids_in_bare(&bare, &["--", "a.txt"]);
    let out = run_grit_bare(grit, &bare, &["log", "--json", "--", "a.txt"]);
    assert!(
        out.status.success(),
        "grit log bare: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let grit_oids: Vec<String> = v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_string())
        .collect();
    assert_eq!(grit_oids, git);
}

#[test]
fn log_pathspec_from_file_in_bare_repo_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let (_wt, bare) = init_bare_two_commit_repo(grit);
    let list = bare.join("paths.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();
    let git = log_oids_in_bare(&bare, &["--", "a.txt"]);
    let out = run_grit_bare(
        grit,
        &bare,
        &["log", "--json", "--pathspec-from-file", list_arg],
    );
    assert!(
        out.status.success(),
        "grit log bare from-file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let grit_oids: Vec<String> = v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_string())
        .collect();
    assert_eq!(grit_oids, git);
}

#[test]
fn diff_pathspec_from_file_in_bare_repo_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let (_wt, bare) = init_bare_two_commit_repo(grit);
    let list = bare.join("paths.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();

    let git_out = Command::new("git")
        .args([
            "--git-dir",
            bare.to_str().unwrap(),
            "diff",
            "--name-only",
            "HEAD~1",
            "HEAD",
            "--",
            "a.txt",
        ])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("git diff bare");
    assert!(git_out.status.success());
    let git_names: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .map(str::to_string)
        .collect();

    let grit_out = run_grit_bare(
        grit,
        &bare,
        &[
            "diff",
            "--name-only",
            "HEAD~1",
            "HEAD",
            "--pathspec-from-file",
            list_arg,
        ],
    );
    assert!(
        grit_out.status.success(),
        "grit diff bare: {}",
        String::from_utf8_lossy(&grit_out.stderr)
    );
    let grit_names: Vec<String> = String::from_utf8_lossy(&grit_out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(grit_names, git_names);
}

fn init_issue_2298_fixture(dir: &std::path::Path) {
    assert!(run_git(dir, &["init", "-b", "main"]).status.success());
    fs::create_dir_all(dir.join("sub")).expect("sub");
    fs::create_dir_all(dir.join("other")).expect("other");
    fs::write(dir.join("sub/f.txt"), b"1\n").expect("sub/f");
    fs::write(dir.join("other/g.txt"), b"1\n").expect("other/g");
    assert!(run_git(dir, &["add", "-A"]).status.success());
    assert!(run_git(dir, &["commit", "-qm", "one"]).status.success());
    fs::OpenOptions::new()
        .append(true)
        .open(dir.join("other/g.txt"))
        .expect("open g")
        .write_all(b"2\n")
        .expect("append g");
    assert!(run_git(dir, &["commit", "-qam", "two"]).status.success());
    fs::OpenOptions::new()
        .append(true)
        .open(dir.join("sub/f.txt"))
        .expect("open f")
        .write_all(b"2\n")
        .expect("append f");
    assert!(run_git(dir, &["commit", "-qam", "three"]).status.success());
}

fn rev_count_git(dir: &std::path::Path, extra: &[&str]) -> u64 {
    let mut args = vec!["rev-list", "--count", "HEAD", "--"];
    args.extend_from_slice(extra);
    let out = run_git(dir, &args);
    assert!(
        out.status.success(),
        "git rev-list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("count")
}

fn rev_count_grit(grit: &str, dir: &std::path::Path, extra: &[&str]) -> u64 {
    let mut args = vec!["revs", "--count", "HEAD", "--"];
    args.extend_from_slice(extra);
    let out = run_grit(grit, dir, &args);
    assert!(
        out.status.success(),
        "grit revs: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("count")
}

fn log_count_grit_json(grit: &str, dir: &std::path::Path, paths: &[&str]) -> usize {
    let mut extra = vec!["--"];
    extra.extend(paths.iter().copied());
    log_oids_grit(grit, dir, &extra).len()
}

fn log_count_git(dir: &std::path::Path, paths: &[&str]) -> usize {
    let mut extra = vec!["--"];
    extra.extend(paths.iter().copied());
    log_oids_git(dir, &extra).len()
}

/// Regression for #2298: cwd-relative pathspecs must not be prefixed twice (CLI + rev walk).
#[test]
fn log_and_revs_trailing_pathspec_from_subdirectory_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_issue_2298_fixture(dir.path());
    let sub = dir.path().join("sub");

    let cases: &[&[&str]] = &[
        &["f.txt"],
        &["."],
        &["./f.txt"],
        &["../sub/f.txt"],
        &[":(top)sub/f.txt"],
        &["*.txt"],
        &[":!f.txt"],
        &[":!g.txt"],
        &[":(glob)**/f.txt"],
    ];

    for paths in cases {
        let git_n = log_count_git(&sub, paths);
        let grit_n = log_count_grit_json(grit, &sub, paths);
        assert_eq!(
            grit_n, git_n,
            "log from sub/ paths {:?}: grit {grit_n} git {git_n}",
            paths
        );

        let git_c = rev_count_git(&sub, paths);
        let grit_c = rev_count_grit(grit, &sub, paths);
        assert_eq!(
            grit_c, git_c,
            "revs --count from sub/ paths {:?}: grit {grit_c} git {git_c}",
            paths
        );
    }

    let list = sub.join("paths.txt");
    fs::write(&list, b"f.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();
    let paths = pathspec_lines_from_file(&list);
    let mut git_args = vec!["--"];
    git_args.extend(paths.iter().map(String::as_str));
    let git_from_file = log_oids_git(&sub, &git_args);
    let grit_from_file = log_oids_grit(grit, &sub, &["--pathspec-from-file", list_arg]);
    assert_eq!(grit_from_file, git_from_file);
}

fn init_issue_2355_fixture(dir: &std::path::Path) {
    assert!(run_git(dir, &["init", "-q"]).status.success());
    assert!(run_git(dir, &["config", "user.email", "a@b"])
        .status
        .success());
    assert!(run_git(dir, &["config", "user.name", "n"]).status.success());
    assert!(run_git(dir, &["config", "commit.gpgsign", "false"])
        .status
        .success());
    fs::create_dir_all(dir.join("a/b")).expect("dirs");
    fs::write(dir.join("a/g.c"), b"1\n").expect("g.c");
    fs::write(dir.join("a/b/h.c"), b"1\n").expect("h.c");
    assert!(run_git(dir, &["add", "-A"]).status.success());
    assert!(run_git(dir, &["commit", "-qm", "init"]).status.success());
    fs::OpenOptions::new()
        .append(true)
        .open(dir.join("a/g.c"))
        .expect("open g")
        .write_all(b"m\n")
        .expect("append g");
    fs::OpenOptions::new()
        .append(true)
        .open(dir.join("a/b/h.c"))
        .expect("open h")
        .write_all(b"m\n")
        .expect("append h");
}

fn stdout_lines(out: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Regression for #2355: long-form `:(top)` pathspecs from a subdirectory match Git.
#[test]
fn top_pathspec_from_subdirectory_matches_git() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    init_issue_2355_fixture(dir.path());
    let sub = dir.path().join("a/b");

    let git_diff = run_git(&sub, &["diff", "--name-only", "--", ":(top)a/g.c"]);
    assert!(git_diff.status.success(), "git diff");
    let git_diff_names = stdout_lines(&git_diff);
    let grit_diff = run_grit(grit, &sub, &["diff", "--name-only", "--", ":(top)a/g.c"]);
    assert!(
        grit_diff.status.success(),
        "grit diff: {}",
        String::from_utf8_lossy(&grit_diff.stderr)
    );
    assert_eq!(stdout_lines(&grit_diff), git_diff_names);

    let git_status = run_git(&sub, &["status", "--short", "--", ":(top)a/g.c"]);
    assert!(git_status.status.success(), "git status");
    let grit_status = run_grit(grit, &sub, &["status", "--short", "--", ":(top)a/g.c"]);
    assert!(
        grit_status.status.success(),
        "grit status: {}",
        String::from_utf8_lossy(&grit_status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_status.stdout),
        String::from_utf8_lossy(&grit_status.stdout)
    );

    assert!(run_git(&sub, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    let git_after_add = ls_files_sorted(dir.path());
    let dir2 = tempfile::TempDir::new().expect("tempdir2");
    init_issue_2355_fixture(dir2.path());
    let sub2 = dir2.path().join("a/b");
    assert!(run_grit(grit, &sub2, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    assert_eq!(ls_files_sorted(dir2.path()), git_after_add);

    let dir3 = tempfile::TempDir::new().expect("tempdir3");
    init_issue_2355_fixture(dir3.path());
    let sub3 = dir3.path().join("a/b");
    assert!(run_git(&sub3, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    assert!(run_git(&sub3, &["restore", "--", ":(top)a/g.c"])
        .status
        .success());
    let git_restored = fs::read_to_string(dir3.path().join("a/g.c")).expect("read g");
    let dir4 = tempfile::TempDir::new().expect("tempdir4");
    init_issue_2355_fixture(dir4.path());
    let sub4 = dir4.path().join("a/b");
    assert!(run_grit(grit, &sub4, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    assert!(run_grit(grit, &sub4, &["restore", "--", ":(top)a/g.c"])
        .status
        .success());
    assert_eq!(
        fs::read_to_string(dir4.path().join("a/g.c")).expect("read g"),
        git_restored
    );

    let dir5 = tempfile::TempDir::new().expect("tempdir5");
    init_issue_2355_fixture(dir5.path());
    let sub5 = dir5.path().join("a/b");
    assert!(run_git(&sub5, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    assert!(run_git(&sub5, &["rm", "--cached", "--", ":(top)a/g.c"])
        .status
        .success());
    let git_rm_index = ls_files_sorted(dir5.path());
    let dir6 = tempfile::TempDir::new().expect("tempdir6");
    init_issue_2355_fixture(dir6.path());
    let sub6 = dir6.path().join("a/b");
    assert!(run_grit(grit, &sub6, &["add", "--", ":(top)a/g.c"])
        .status
        .success());
    assert!(
        run_grit(grit, &sub6, &["rm", "--cached", "--", ":(top)a/g.c"])
            .status
            .success()
    );
    assert_eq!(ls_files_sorted(dir6.path()), git_rm_index);

    let git_stash = run_git(&sub, &["stash", "push", "--", ":(top,glob)a/*.c"]);
    assert!(git_stash.status.success(), "git stash");
    let dir7 = tempfile::TempDir::new().expect("tempdir7");
    init_issue_2355_fixture(dir7.path());
    let sub7 = dir7.path().join("a/b");
    let grit_stash = run_grit(grit, &sub7, &["stash", "push", "--", ":(top,glob)a/*.c"]);
    assert!(
        grit_stash.status.success(),
        "grit stash: {}",
        String::from_utf8_lossy(&grit_stash.stderr)
    );

    let git_range = run_git(
        &sub,
        &[
            "diff",
            "--name-only",
            "HEAD~1",
            "HEAD",
            "--",
            ":(top,glob)a/**/*.c",
        ],
    );
    assert!(git_range.status.success(), "git range diff");
    let grit_range = run_grit(
        grit,
        &sub,
        &[
            "diff",
            "--name-only",
            "HEAD~1",
            "HEAD",
            "--",
            ":(top,glob)a/**/*.c",
        ],
    );
    assert!(
        grit_range.status.success(),
        "grit range diff: {}",
        String::from_utf8_lossy(&grit_range.stderr)
    );
    assert_eq!(stdout_lines(&grit_range), stdout_lines(&git_range));
}

#[test]
fn show_pathspec_from_file_in_bare_repo_succeeds() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let (_wt, bare) = init_bare_two_commit_repo(grit);
    let list = bare.join("paths.txt");
    fs::write(&list, b"a.txt\n").expect("write list");
    let list_arg = list.to_str().unwrap();
    let out = run_grit_bare(
        grit,
        &bare,
        &[
            "show",
            "HEAD",
            "--name-only",
            "--pathspec-from-file",
            list_arg,
        ],
    );
    assert!(
        out.status.success(),
        "grit show bare: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("a.txt"));
}

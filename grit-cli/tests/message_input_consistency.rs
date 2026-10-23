//! Shared `-m` / `-F` message input produces identical object bytes across mutating commands.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const GS: &str = env!("CARGO_BIN_EXE_grit");

/// File payload with extra trailing whitespace (normalized by grit-lib).
const MSG_FILE: &str = "  hello unified message  \n\n";
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git")
}

fn gs(dir: &Path, args: &[&str]) -> Output {
    Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GRIT_CHANGE_IDS", "false")
        .output()
        .expect("grit")
}

fn gs_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GRIT_CHANGE_IDS", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn init_repo() -> PathBuf {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    std::mem::forget(tmp);
    root
}

fn commit_body(root: &Path) -> String {
    String::from_utf8(git(root, &["log", "-1", "--format=%B"]).stdout).unwrap()
}

fn write_msg_file(root: &Path) {
    fs::write(root.join("msg.txt"), MSG_FILE).unwrap();
}

fn tag_message_body(root: &Path) -> String {
    let raw = String::from_utf8(git(root, &["cat-file", "-p", "v^{tag}"]).stdout).unwrap();
    raw.split("\n\n").nth(1).unwrap_or("").to_owned()
}

fn note_body(root: &Path) -> String {
    String::from_utf8(git(root, &["notes", "show", "HEAD"]).stdout).unwrap()
}

#[test]
fn commit_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    fs::write(root.join("f.txt"), "1\n").unwrap();
    git(&root, &["add", "f.txt"]);

    let run = |mode: &str| {
        fs::write(root.join("f.txt"), format!("{mode}\n")).unwrap();
        git(&root, &["add", "f.txt"]);
        match mode {
            "m" => assert!(gs(&root, &["commit", "-m", MSG_FILE, "--staged"])
                .status
                .success()),
            "f" => assert!(gs(&root, &["commit", "-F", "msg.txt", "--staged"])
                .status
                .success()),
            _ => assert!(gs_stdin(
                &root,
                &["commit", "-F", "-", "--staged"],
                MSG_FILE.as_bytes()
            )
            .status
            .success()),
        }
        commit_body(&root)
    };
    let via_m = run("m");
    let via_f = run("f");
    let via_stdin = run("stdin");
    assert_eq!(via_m, via_f);
    assert_eq!(via_m, via_stdin);
}

#[test]
fn merge_no_ff_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    fs::write(root.join("base"), "b\n").unwrap();
    git(&root, &["add", "base"]);
    git(&root, &["commit", "-m", "base"]);
    git(&root, &["branch", "topic"]);
    git(&root, &["checkout", "topic"]);
    fs::write(root.join("topic"), "t\n").unwrap();
    git(&root, &["add", "topic"]);
    git(&root, &["commit", "-m", "topic"]);
    git(&root, &["checkout", "main"]);
    fs::write(root.join("mainline"), "m\n").unwrap();
    git(&root, &["add", "mainline"]);
    git(&root, &["commit", "-m", "mainline"]);

    let merge = |mode: &str| {
        git(&root, &["reset", "--hard", "main"]);
        match mode {
            "m" => assert!(gs(&root, &["merge", "--no-ff", "-m", MSG_FILE, "topic"])
                .status
                .success()),
            "f" => assert!(gs(&root, &["merge", "--no-ff", "-F", "msg.txt", "topic"])
                .status
                .success()),
            _ => assert!(gs_stdin(
                &root,
                &["merge", "--no-ff", "-F", "-", "topic"],
                MSG_FILE.as_bytes(),
            )
            .status
            .success()),
        }
        commit_body(&root)
    };
    assert_eq!(merge("m"), merge("f"));
    assert_eq!(merge("m"), merge("stdin"));
}

#[test]
fn pick_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    fs::write(root.join("a"), "1\n").unwrap();
    git(&root, &["add", "a"]);
    git(&root, &["commit", "-m", "root"]);
    git(&root, &["branch", "side"]);
    git(&root, &["checkout", "side"]);
    fs::write(root.join("b"), "2\n").unwrap();
    git(&root, &["add", "b"]);
    git(&root, &["commit", "-m", "side-change"]);
    let picked = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&root, &["checkout", "main"]);
    let main_tip = String::from_utf8(git(&root, &["rev-parse", "main"]).stdout)
        .unwrap()
        .trim()
        .to_owned();

    let pick = |mode: &str| {
        git(&root, &["reset", "--hard", &main_tip]);
        match mode {
            "m" => assert!(gs(&root, &["pick", "-m", MSG_FILE, &picked])
                .status
                .success()),
            "f" => assert!(gs(&root, &["pick", "-F", "msg.txt", &picked])
                .status
                .success()),
            _ => assert!(
                gs_stdin(&root, &["pick", "-F", "-", &picked], MSG_FILE.as_bytes())
                    .status
                    .success()
            ),
        }
        commit_body(&root)
    };
    assert_eq!(pick("m"), pick("f"));
    assert_eq!(pick("m"), pick("stdin"));
}

#[test]
fn revert_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    fs::write(root.join("a"), "1\n").unwrap();
    git(&root, &["add", "a"]);
    git(&root, &["commit", "-m", "target"]);
    let target = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&root, &["commit", "--allow-empty", "-m", "keep"]);
    let main_tip = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();

    let revert = |mode: &str| {
        git(&root, &["reset", "--hard", &main_tip]);
        match mode {
            "m" => assert!(gs(&root, &["revert", "-m", MSG_FILE, &target])
                .status
                .success()),
            "f" => assert!(gs(&root, &["revert", "-F", "msg.txt", &target])
                .status
                .success()),
            _ => assert!(
                gs_stdin(&root, &["revert", "-F", "-", &target], MSG_FILE.as_bytes())
                    .status
                    .success()
            ),
        }
        commit_body(&root)
    };
    assert_eq!(revert("m"), revert("f"));
    assert_eq!(revert("m"), revert("stdin"));
}

#[test]
fn tag_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    git(&root, &["commit", "--allow-empty", "-m", "head"]);

    let tag = |mode: &str| {
        let _ = git(&root, &["tag", "-d", "v"]);
        match mode {
            "m" => assert!(gs(&root, &["tag", "v", "-m", MSG_FILE]).status.success()),
            "f" => assert!(gs(&root, &["tag", "v", "-F", "msg.txt"]).status.success()),
            _ => assert!(
                gs_stdin(&root, &["tag", "v", "-F", "-"], MSG_FILE.as_bytes())
                    .status
                    .success()
            ),
        }
        tag_message_body(&root)
    };
    assert_eq!(tag("m"), tag("f"));
    assert_eq!(tag("m"), tag("stdin"));
}

#[test]
fn notes_add_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    git(&root, &["commit", "--allow-empty", "-m", "head"]);

    let note = |mode: &str| {
        let _ = gs(&root, &["notes", "remove", "HEAD"]);
        match mode {
            "m" => assert!(gs(&root, &["notes", "add", "HEAD", "-m", MSG_FILE])
                .status
                .success()),
            "f" => assert!(gs(&root, &["notes", "add", "HEAD", "-F", "msg.txt"])
                .status
                .success()),
            _ => assert!(gs_stdin(
                &root,
                &["notes", "add", "HEAD", "-F", "-"],
                MSG_FILE.as_bytes()
            )
            .status
            .success()),
        }
        note_body(&root)
    };
    assert_eq!(note("m"), note("f"));
    assert_eq!(note("m"), note("stdin"));
}

#[test]
fn history_reword_message_input_paths_match() {
    let root = init_repo();
    write_msg_file(&root);
    git(&root, &["commit", "--allow-empty", "-m", "old"]);
    let oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();

    let reword = |mode: &str| {
        git(&root, &["reset", "--hard", &oid]);
        match mode {
            "m" => assert!(gs(&root, &["history", "reword", &oid, "-m", MSG_FILE])
                .status
                .success()),
            "f" => assert!(gs(&root, &["history", "reword", &oid, "-F", "msg.txt"])
                .status
                .success()),
            _ => assert!(gs_stdin(
                &root,
                &["history", "reword", &oid, "-F", "-"],
                MSG_FILE.as_bytes()
            )
            .status
            .success()),
        }
        commit_body(&root)
    };
    assert_eq!(reword("m"), reword("f"));
    assert_eq!(reword("m"), reword("stdin"));
}

#[test]
fn message_and_message_file_together_is_usage_exit_2_json() {
    let root = init_repo();
    fs::write(root.join("m.txt"), "x\n").unwrap();
    let out = gs(
        &root,
        &[
            "commit",
            "-m",
            "one",
            "-F",
            "m.txt",
            "--json",
            "--allow-empty",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(err["kind"], "usage");
}

#[test]
fn malformed_author_is_usage_on_commit_and_reword() {
    let root = init_repo();
    git(&root, &["commit", "--allow-empty", "-m", "seed"]);
    let seed = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();

    let bad_commit = gs(
        &root,
        &[
            "commit",
            "-m",
            "next",
            "--author",
            "not-an-email",
            "--allow-empty",
            "--json",
        ],
    );
    assert_eq!(bad_commit.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&bad_commit.stdout).unwrap();
    assert_eq!(err["kind"], "usage");

    let bad_reword = gs(
        &root,
        &[
            "history",
            "reword",
            &seed,
            "--author",
            "not-an-email",
            "--json",
        ],
    );
    assert_eq!(bad_reword.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&bad_reword.stdout).unwrap();
    assert_eq!(err["kind"], "usage");
}

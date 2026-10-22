//! Change id fields in log/show/revs/status JSON and human log/show output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::change_id::{resolved_change_id, ChangeId};
use grit_lib::objects::ObjectId;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn temp_repo(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "grit-change-id-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("mkdir");
    base
}

fn grit_env(cmd: &mut Command) {
    cmd.env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::new(GRIT);
    grit_env(&mut cmd);
    let out = cmd
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "grit --json {} failed\nstdout={}\nstderr={}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn grit_text(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(GRIT);
    grit_env(&mut cmd);
    let out = cmd
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_with_git_commit(repo: &Path) -> ObjectId {
    Command::new(GRIT)
        .args(["init", "."])
        .current_dir(repo)
        .status()
        .expect("grit init");
    fs::write(repo.join("f.txt"), "hello\n").expect("write");
    Command::new("git")
        .args(["add", "f.txt"])
        .current_dir(repo)
        .status()
        .expect("git add");
    Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args([
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "seed",
        ])
        .current_dir(repo)
        .status()
        .expect("git commit");
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git rev-parse")
        .stdout
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .and_then(|hex| hex.parse().ok())
        .expect("head oid")
}

fn git_must(repo: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed (cwd {}): {}",
        args.join(" "),
        repo.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn git_rev_parse(repo: &Path, spec: &str) -> String {
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", spec])
            .current_dir(repo)
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .trim()
    .to_owned()
}

fn write_stored_change_commit(repo: &Path) -> (ObjectId, ChangeId) {
    let parent: ObjectId = git_rev_parse(repo, "HEAD").parse().expect("head oid");
    let tree: ObjectId = git_rev_parse(repo, "HEAD^{tree}")
        .parse()
        .expect("tree oid");
    let stored = ChangeId::from_bytes(&[0x42; 16]);
    let ident = b"Test User <test@example.com> 1700000000 +0000";
    let commit = CommitData {
        tree,
        parents: vec![parent],
        author: String::from_utf8_lossy(ident).into_owned(),
        committer: String::from_utf8_lossy(ident).into_owned(),
        author_raw: ident.to_vec(),
        committer_raw: ident.to_vec(),
        encoding: None,
        message: "stored header".to_owned(),
        raw_message: None,
        extra_headers: format!("change-id {}\n", stored.as_str()).into_bytes(),
    };
    let body = serialize_commit(&commit);
    let data = parse_commit(&body).expect("parse serialized commit");
    let resolved = resolved_change_id(&parent, &data);
    assert!(
        resolved.stored,
        "serialized commit must carry a parsable change-id header"
    );
    let mut hash = Command::new("git")
        .args(["hash-object", "-w", "--stdin", "-t", "commit"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git hash-object");
    hash.stdin
        .as_mut()
        .expect("stdin")
        .write_all(&body)
        .expect("write");
    let out = hash.wait_with_output().expect("wait");
    assert!(out.status.success());
    let oid: ObjectId = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("oid");
    git_must(repo, &["update-ref", "HEAD", &oid.to_hex()]).expect("point HEAD at stored commit");
    (oid, stored)
}

fn assert_derived_fields(commit: &Value) {
    let oid: ObjectId = commit["oid"].as_str().expect("oid").parse().expect("parse");
    assert_eq!(commit["change_id_stored"], false);
    assert_eq!(
        commit["change_id"].as_str().expect("change_id"),
        ChangeId::derived_from_commit_id(&oid).as_str()
    );
}

fn assert_grit_stored_change_id(commit: &Value) {
    assert_eq!(commit["change_id_stored"], true);
    let id = commit["change_id"].as_str().expect("change_id");
    assert!(id.len() >= 8, "change_id should be non-trivial, got {id}");
}

#[test]
fn log_json_has_change_id() -> TestResult {
    let repo = temp_repo("log-json");
    init_with_git_commit(&repo);
    let log = grit_json(&repo, &["log", "--limit", "1"]);
    assert_derived_fields(&log["commits"][0]);
    let (stored_oid, stored_id) = write_stored_change_commit(&repo);
    let log = grit_json(&repo, &["log", "--limit", "1"]);
    assert_eq!(
        log["commits"][0]["oid"].as_str(),
        Some(stored_oid.to_hex().as_str())
    );
    assert_eq!(log["commits"][0]["change_id_stored"], true);
    assert_eq!(
        log["commits"][0]["change_id"].as_str(),
        Some(stored_id.as_str())
    );
    Ok(())
}

#[test]
fn show_json_has_change_id() -> TestResult {
    let repo = temp_repo("show-json");
    init_with_git_commit(&repo);
    assert_derived_fields(&grit_json(&repo, &["show"])["commit"]);
    Ok(())
}

#[test]
fn revs_json_has_change_id() -> TestResult {
    let repo = temp_repo("revs-json");
    init_with_git_commit(&repo);
    assert_derived_fields(&grit_json(&repo, &["revs", "HEAD"])["commits"][0]);
    Ok(())
}

#[test]
fn status_json_commits_have_change_id() -> TestResult {
    let root = temp_repo("status-json");
    let bare = root.join("remote.git");
    let bare_s = bare.to_str().expect("utf8");
    git_must(&root, &["init", "--bare", "-q", bare_s])?;
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git_must(&upstream, &["clone", "-q", bare_s, "."])?;
    git_must(&upstream, &["config", "user.name", "Test User"])?;
    git_must(&upstream, &["config", "user.email", "test@example.com"])?;
    fs::write(upstream.join("f.txt"), "one\n")?;
    git_must(&upstream, &["add", "f.txt"])?;
    git_must(&upstream, &["commit", "-qm", "one"])?;
    git_must(&upstream, &["branch", "-M", "main"])?;
    git_must(&upstream, &["push", "-u", "origin", "main"])?;

    let repo = root.join("repo");
    git_must(
        &root,
        &[
            "clone",
            "-q",
            "-b",
            "main",
            bare_s,
            repo.to_str().expect("utf8"),
        ],
    )?;
    fs::write(repo.join("f.txt"), "two\n")?;
    let mut second = Command::new(GRIT);
    grit_env(&mut second);
    let commit_out = second
        .args(["commit", "-m", "two"])
        .current_dir(&repo)
        .output()?;
    assert!(
        commit_out.status.success(),
        "grit commit failed: {}",
        String::from_utf8_lossy(&commit_out.stderr)
    );
    let status = grit_json(&repo, &["status"]);
    let commits = status["commits"].as_array().expect("commits");
    assert!(!commits.is_empty());
    for row in commits {
        assert_grit_stored_change_id(row);
    }
    Ok(())
}

#[test]
fn log_human_shows_short_change_id() -> TestResult {
    let repo = temp_repo("log-human");
    init_with_git_commit(&repo);
    let out = grit_text(&repo, &["log", "--limit", "1"]);
    let short: String = grit_json(&repo, &["log", "--limit", "1"])["commits"][0]["change_id"]
        .as_str()
        .expect("change_id")
        .chars()
        .take(8)
        .collect();
    assert!(out.contains(&format!("change {short}")));
    Ok(())
}

#[test]
fn oneline_unchanged() -> TestResult {
    let repo = temp_repo("oneline");
    init_with_git_commit(&repo);
    let out = grit_text(&repo, &["log", "--oneline", "--limit", "1"]);
    assert!(!out.contains("change "));
    Ok(())
}

//! `grit log` author display matches commit author name (issue #969).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Zed Quartz")
        .env("GIT_AUTHOR_EMAIL", "zed@q.test")
        .env("GIT_COMMITTER_NAME", "Zed Quartz")
        .env("GIT_COMMITTER_EMAIL", "zed@q.test")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn grit_ok(dir: &Path, args: &[&str]) {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Zed Quartz")
        .env("GIT_AUTHOR_EMAIL", "zed@q.test")
        .env("GIT_COMMITTER_NAME", "Zed Quartz")
        .env("GIT_COMMITTER_EMAIL", "zed@q.test")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_author_name(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%an"])
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert_eq!(out.status.code(), Some(0));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn log_author_matches_show_and_git() -> TestResult {
    let base = std::env::temp_dir().join(format!(
        "grit-log-author-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&base)?;
    let repo = base.join("repo");
    fs::create_dir_all(&repo)?;

    grit_ok(&repo, &["init", "."]);
    fs::write(repo.join("f"), "a\n")?;
    grit_ok(&repo, &["commit", "-m", "one"]);

    let log = grit_json(&repo, &["log"]);
    let show = grit_json(&repo, &["show"]);
    let show_author = &show["commit"]["author"];
    assert_eq!(show_author["name"], "Zed Quartz");
    assert_eq!(show_author["email"], "zed@q.test");

    assert_eq!(log["commits"][0]["author"], *show_author);
    assert_eq!(git_author_name(&repo), "Zed Quartz");

    let human = Command::new(GRIT)
        .args(["log"])
        .current_dir(&repo)
        .output()?;
    assert_eq!(human.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("Zed Quartz"),
        "human log should show author name, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(" zed "),
        "human log should not show email local-part, got:\n{stdout}"
    );

    fs::remove_dir_all(&base).ok();
    Ok(())
}

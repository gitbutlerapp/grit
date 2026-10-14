//! `grit status --untracked` lists individual files when set to `all`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_json(repo: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(GRIT)
        .args(args)
        .arg("--json")
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn git_lines(repo: &Path, args: &[&str]) -> Vec<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn status_untracked_all_lists_every_file_and_matches_git() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path().join("r");
    fs::create_dir_all(repo.join("docs"))?;
    fs::create_dir_all(repo.join("src"))?;
    fs::write(repo.join("docs/a.txt"), "a\n")?;
    fs::write(repo.join("docs/b.txt"), "b\n")?;
    fs::write(repo.join("src/main.rs"), "fn main() {}\n")?;
    fs::write(repo.join("plain.txt"), "p\n")?;

    Command::new(GRIT)
        .args(["init", "."])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("grit init");

    let collapsed = grit_json(&repo, &["status"]);
    let collapsed_set: std::collections::BTreeSet<_> = collapsed["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v.as_str()
                .expect("untracked path")
                .trim_end_matches('/')
                .to_owned()
        })
        .collect();
    assert_eq!(
        collapsed_set,
        ["docs", "plain.txt", "src"]
            .into_iter()
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    );

    let expanded = grit_json(&repo, &["status", "--untracked", "all"]);
    let grit_untracked: std::collections::BTreeSet<String> = expanded["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();

    let git_untracked: std::collections::BTreeSet<String> =
        git_lines(&repo, &["ls-files", "-o", "--exclude-standard"])
            .into_iter()
            .collect();

    assert_eq!(grit_untracked, git_untracked);
    assert!(grit_untracked.contains("docs/a.txt"));
    assert!(grit_untracked.contains("docs/b.txt"));
    assert!(grit_untracked.contains("src/main.rs"));
    assert!(grit_untracked.contains("plain.txt"));

    Ok(())
}

#[test]
fn status_untracked_no_omits_untracked_paths() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("lonely.txt"), "x\n")?;
    Command::new(GRIT)
        .args(["init", "."])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("grit init");

    let v = grit_json(repo, &["status", "--untracked", "no"]);
    assert_eq!(v["untracked"].as_array().unwrap().len(), 0);
    assert_eq!(v["clean"], serde_json::Value::Bool(true));
    Ok(())
}

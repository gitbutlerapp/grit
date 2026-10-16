//! `grit status --untracked` lists individual files when set to `all`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_test_support::configure_git_env;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_cmd() -> Command {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd
}

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    cmd
}

fn grit_json(repo: &Path, args: &[&str]) -> serde_json::Value {
    let out = grit_cmd()
        .args(args)
        .arg("--json")
        .current_dir(repo)
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

fn grit_ok(repo: &Path, args: &[&str]) {
    let out = grit_cmd()
        .args(args)
        .current_dir(repo)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_ok(repo: &Path, args: &[&str]) {
    let out = git_cmd()
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_untracked_set(repo: &Path) -> std::collections::BTreeSet<String> {
    git_lines(repo, &["ls-files", "-o", "--exclude-standard"])
        .into_iter()
        .collect()
}

fn grit_untracked_set(repo: &Path, args: &[&str]) -> std::collections::BTreeSet<String> {
    grit_json(repo, args)["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn git_lines(repo: &Path, args: &[&str]) -> Vec<String> {
    let out = git_cmd()
        .args(args)
        .current_dir(repo)
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

    grit_cmd()
        .args(["init", "."])
        .current_dir(&repo)
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

fn seed_untracked_tree(repo: &Path) -> TestResult {
    fs::create_dir_all(repo.join("docs"))?;
    fs::create_dir_all(repo.join("src"))?;
    fs::write(repo.join("docs/a.txt"), "a\n")?;
    fs::write(repo.join("docs/b.txt"), "b\n")?;
    fs::write(repo.join("src/main.rs"), "fn main() {}\n")?;
    fs::write(repo.join("plain.txt"), "p\n")?;
    Ok(())
}

#[test]
fn status_untracked_all_after_untracked_cache_normal_mode() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path().join("r");
    fs::create_dir_all(&repo)?;
    grit_cmd()
        .args(["init", "."])
        .current_dir(&repo)
        .status()
        .expect("grit init");
    grit_ok(&repo, &["config", "core.untrackedCache", "true"]);
    seed_untracked_tree(&repo)?;
    git_ok(&repo, &["update-index", "--untracked-cache"]);

    let _ = grit_json(&repo, &["status"]);
    let expanded = grit_untracked_set(&repo, &["status", "--untracked", "all"]);
    assert_eq!(expanded, git_untracked_set(&repo));

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
    Ok(())
}

#[test]
fn status_untracked_all_from_config_without_flag() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path().join("r");
    fs::create_dir_all(&repo)?;
    grit_cmd()
        .args(["init", "."])
        .current_dir(&repo)
        .status()
        .expect("grit init");
    grit_ok(&repo, &["config", "status.showUntrackedFiles", "all"]);
    seed_untracked_tree(&repo)?;

    let grit_set = grit_untracked_set(&repo, &["status"]);
    assert_eq!(grit_set, git_untracked_set(&repo));
    Ok(())
}

#[test]
fn status_rejects_invalid_show_untracked_files_config_like_git() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("lonely.txt"), "x\n")?;
    grit_cmd()
        .args(["init", "."])
        .current_dir(repo)
        .status()
        .expect("grit init");
    grit_ok(repo, &["config", "status.showUntrackedFiles", "false"]);

    let grit = grit_cmd()
        .args(["status", "--json"])
        .current_dir(repo)
        .output()
        .expect("grit status");

    let git = git_cmd()
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()
        .expect("git status");

    assert_eq!(
        grit.status.success(),
        git.status.success(),
        "grit and git must agree on invalid showUntrackedFiles=false (grit stdout: {}, git stderr: {})",
        String::from_utf8_lossy(&grit.stdout),
        String::from_utf8_lossy(&git.stderr)
    );
    assert!(
        !grit.status.success(),
        "grit must reject invalid showUntrackedFiles like git:\n{}",
        String::from_utf8_lossy(&grit.stdout)
    );
    Ok(())
}

#[test]
fn status_untracked_no_omits_untracked_paths() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("lonely.txt"), "x\n")?;
    grit_cmd()
        .args(["init", "."])
        .current_dir(repo)
        .status()
        .expect("grit init");

    let v = grit_json(repo, &["status", "--untracked", "no"]);
    assert_eq!(v["untracked"].as_array().unwrap().len(), 0);
    assert_eq!(v["clean"], serde_json::Value::Bool(true));
    Ok(())
}

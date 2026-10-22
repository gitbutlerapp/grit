//! `grit tag` list flags vs system `git tag`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn grit_names(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["tag"];
    argv.extend(args);
    let out = Command::new(GRIT)
        .args(&argv)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
        .collect()
}

fn git_names(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["tag", "-l"];
    argv.extend(args);
    git(dir, &argv)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn seed_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "--allow-empty", "-q", "-m", "c1"]);
    git(dir, &["commit", "--allow-empty", "-q", "-m", "c2"]);
    git(dir, &["commit", "--allow-empty", "-q", "-m", "c3"]);
    for name in ["1.9", "1.10", "v1.0", "v1.0-rc1", "feature/foo"] {
        git(dir, &["tag", name]);
    }
    git(dir, &["tag", "-a", "annotated", "-m", "note", "HEAD~1"]);
    git(dir, &["tag", "-a", "inner", "-m", "i", "HEAD~2"]);
    git(dir, &["tag", "-a", "outer", "-m", "o", "inner"]);
}

#[test]
fn list_sort_and_filters_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    seed_repo(dir);

    assert_eq!(
        grit_names(dir, &[]),
        git_names(dir, &["--sort=version:refname"])
    );
    assert_eq!(
        grit_names(dir, &["--sort=version"]),
        git_names(dir, &["--sort=version:refname"])
    );
    assert_eq!(
        grit_names(dir, &["--sort=date", "--reverse"]),
        git_names(dir, &["--sort=-creatordate"])
    );
    assert_eq!(
        grit_names(dir, &["--sort=name"]),
        git_names(dir, &["--sort=refname"])
    );
    assert_eq!(
        grit_names(dir, &["--contains", "HEAD~1"]),
        git_names(dir, &["--contains", "HEAD~1", "--sort=version:refname"])
    );
    assert_eq!(
        grit_names(dir, &["--points-at", "HEAD~2"]),
        git_names(dir, &["--points-at", "HEAD~2", "--sort=version:refname"])
    );
    assert_eq!(
        grit_names(dir, &["-l", "v*"]),
        git_names(dir, &["v*", "--sort=version:refname"])
    );
    let limited = grit_names(dir, &["-n", "2", "--sort=name"]);
    assert_eq!(limited.len(), 2);
    let name_sorted = git_names(dir, &["--sort=refname"]);
    assert_eq!(limited, name_sorted.into_iter().take(2).collect::<Vec<_>>());
}

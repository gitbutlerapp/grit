//! Cwd-relative untracked paths in `grit status` match Git (issue #2344).

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

fn short_status_lines(repo: &Path, tool: &str, extra: &[&str]) -> Vec<String> {
    let mut cmd = if tool == "grit" {
        grit_cmd()
    } else {
        git_cmd()
    };
    let out = cmd.args(extra).current_dir(repo).output().expect(tool);
    assert!(
        out.status.success(),
        "{tool} status failed:\n{}",
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

fn grit_json_untracked(repo: &Path) -> Vec<String> {
    let out = grit_cmd()
        .args(["status", "--json"])
        .current_dir(repo)
        .output()
        .expect("grit");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn status_untracked_paths_match_git_from_subdirs() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@e.x"]);
    git_ok(root, &["config", "user.name", "T"]);
    git_ok(root, &["commit", "-q", "--allow-empty", "-m", "i"]);

    let newdir = root.join("newdir");
    fs::create_dir_all(&newdir)?;
    fs::write(newdir.join("f"), b"a\n")?;
    fs::write(newdir.join("g"), b"b\n")?;

    let other_sub = root.join("other").join("sub");
    fs::create_dir_all(&other_sub)?;
    fs::write(other_sub.join("h"), b"x\n")?;

    let other = root.join("other");
    let cwds = [
        ("newdir", newdir.as_path()),
        ("other", other.as_path()),
        ("other/sub", other_sub.as_path()),
    ];

    for (label, cwd) in cwds {
        let grit_short = short_status_lines(cwd, "grit", &["status", "-s"]);
        let git_short = short_status_lines(cwd, "git", &["status", "-s"]);
        assert_eq!(grit_short, git_short, "short status mismatch from {label}");

        let grit_md = short_status_lines(cwd, "grit", &["status", "--markdown"]);
        let grit_long = grit_cmd()
            .args(["status"])
            .current_dir(cwd)
            .output()
            .expect("grit long");
        assert!(grit_long.status.success());
        let long_body = String::from_utf8(grit_long.stdout)?;
        for line in &grit_short {
            let path = line.strip_prefix("?? ").unwrap_or(line);
            assert!(
                grit_md.iter().any(|l| l.contains(path)) || long_body.contains(path),
                "path {path:?} missing from long/markdown output from {label}"
            );
        }
    }

    let json_from_newdir = grit_json_untracked(newdir.as_path());
    assert_eq!(
        json_from_newdir,
        vec!["newdir/".to_owned(), "other/".to_owned()]
    );

    let json_from_sub = grit_json_untracked(other_sub.as_path());
    assert_eq!(
        json_from_sub,
        vec!["newdir/".to_owned(), "other/".to_owned()]
    );

    Ok(())
}

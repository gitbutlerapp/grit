//! `grit log` / `grit revs` `--reverse -n` window matches system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut argv = vec!["-C", dir.to_str().expect("utf8")];
    argv.extend(args);
    let out = Command::new("git")
        .args(&argv)
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

fn grit_run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit")
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
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

fn seed_five_empty_commits(dir: &Path) {
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("git init")
        .success());
    for i in 1..=5 {
        assert!(Command::new("git")
            .args(["commit", "--allow-empty", "-qm", &format!("c{i}")])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .status()
            .expect("git commit")
            .success());
    }
}

fn git_log_subjects(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--format=%s"];
    args.extend(extra);
    git(dir, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_log_subjects(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--oneline"];
    args.extend(extra);
    let out = grit_run(dir, &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('→'))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            parts.next()?; // short oid
            Some(parts.collect::<Vec<_>>().join(" "))
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn git_log_oids(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--format=%H"];
    args.extend(extra);
    git(dir, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn log_reverse_limit_matches_git_on_linear_repo() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-reverse-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    seed_five_empty_commits(&dir);

    let git_subjects = git_log_subjects(&dir, &["--reverse", "-n", "3"]);
    let grit_subjects = grit_log_subjects(&dir, &["--reverse", "-n", "3"]);
    assert_eq!(grit_subjects, git_subjects, "expected c3 c4 c5 window");

    let git_skip = git_log_subjects(&dir, &["--reverse", "-n", "3", "--skip", "1"]);
    let grit_skip = grit_log_subjects(&dir, &["--reverse", "-n", "3", "--skip", "1"]);
    assert_eq!(grit_skip, git_skip);

    let git_oids = git_log_oids(&dir, &["--reverse", "-n", "3"]);
    let json = grit_json(&dir, &["log", "--reverse", "-n", "3"]);
    assert_eq!(json["truncated"].as_bool(), Some(true));
    let grit_oids: Vec<String> = json["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(grit_oids, git_oids);

    let revs_json = grit_json(&dir, &["revs", "--reverse", "-n", "3", "HEAD"]);
    let revs_oids: Vec<String> = revs_json["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(revs_oids, git_oids);

    Ok(())
}

#[test]
fn log_reverse_limit_with_date_window_matches_git_first_oid() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-reverse-date-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("init")
        .success());
    for i in 0..30 {
        let day = (i % 28) + 1;
        let when = format!("2024-01-{day:02} 12:00:00 +0000");
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=dev@example.com",
                "-c",
                "user.name=Dev",
                "commit",
                "--allow-empty",
                "-qm",
                &format!("m{i}"),
            ])
            .current_dir(&dir)
            .env("GIT_AUTHOR_DATE", &when)
            .env("GIT_COMMITTER_DATE", &when)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .status()
            .expect("commit")
            .success());
    }

    let filters = [
        "--reverse",
        "-n",
        "20",
        "--since=2024-01-01",
        "--until=2024-06-01",
    ];
    let git_first = git_log_oids(&dir, &filters)
        .first()
        .cloned()
        .expect("git oids");
    let grit_first = grit_json(
        &dir,
        &[
            "log",
            "--reverse",
            "-n",
            "20",
            "--since=2024-01-01",
            "--until=2024-06-01",
        ],
    )["commits"]
        .as_array()
        .expect("commits")
        .first()
        .and_then(|c| c["oid"].as_str())
        .expect("oid")
        .to_owned();
    assert_eq!(
        grit_first, git_first,
        "oldest commit in reverse -n window should match git"
    );
    Ok(())
}

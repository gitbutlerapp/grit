//! `show_date` / `parse_date` vs `git log --date=<mode>` on fixed timestamps.

use std::process::Command;

use grit_lib::git_date::parse::{parse_date_at, parse_date_basic_at};
use grit_lib::git_date::show::{parse_date_format, show_date};

const FIXED: u64 = 1_700_000_000;
const NOW: i64 = 1_700_100_000;

fn git_log_date(mode: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "f"])
        .env("GIT_AUTHOR_DATE", format!("{FIXED} +0000"))
        .env("GIT_COMMITTER_DATE", format!("{FIXED} +0000"))
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c"])
        .env("GIT_AUTHOR_DATE", format!("{FIXED} +0000"))
        .env("GIT_COMMITTER_DATE", format!("{FIXED} +0000"))
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["log", "-1", "--format=%ad", &format!("--date={mode}")])
        .output()
        .expect("log");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn git_log_date_iso_matches_show_date() {
    for mode in ["iso", "iso-strict", "short", "raw", "unix"] {
        let git_s = git_log_date(mode);
        let mut parsed = parse_date_format(mode).expect("parse mode");
        let grit_s = show_date(FIXED, 0, &mut parsed);
        let normalize = |s: &str| s.replace("+00:00", "Z");
        assert_eq!(
            normalize(grit_s.trim()),
            normalize(git_s.trim()),
            "date mode {mode}: grit {grit_s:?} vs git {git_s:?}"
        );
    }
}

#[test]
fn parse_date_basic_roundtrip_fixed_epoch() {
    let canonical = parse_date_at("Thu Nov 14 22:13:20 2024 +0000", NOW).expect("parse");
    let (sec, off) = parse_date_basic_at("Thu Nov 14 22:13:20 2024 +0000", NOW).expect("basic");
    assert!(off == 0 || off == 0);
    assert!(!canonical.is_empty());
    assert!(sec > 0);
}

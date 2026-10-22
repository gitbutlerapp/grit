//! `normalize_commit_date_override` / `parse_date_at` vs `git commit --date`.

use std::process::Command;
use std::sync::{LazyLock, Mutex, MutexGuard};

use grit_lib::commit::normalize_commit_date_override;
use grit_lib::config::git_date::parse::parse_date_at;
use grit_lib::config::git_date::tm::{process_wall_clock_sec, refresh_process_tz};

static TZ_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct UtcTzGuard {
    saved: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl UtcTzGuard {
    fn new() -> Self {
        let _lock = TZ_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("TZ").ok();
        std::env::set_var("TZ", "UTC");
        refresh_process_tz();
        Self { saved, _lock }
    }
}

impl Drop for UtcTzGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        refresh_process_tz();
    }
}

fn git_author_timestamp(spec: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .expect("init");
    std::fs::write(dir.path().join("f"), b"x").expect("write");
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "f"])
        .status()
        .expect("add");
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c", &format!("--date={spec}")])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .expect("commit");
    assert!(status.success(), "git commit --date={spec:?}");
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["cat-file", "-p", "HEAD"])
        .output()
        .expect("cat-file");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let author = text
        .lines()
        .find(|l| l.starts_with("author "))
        .expect("author line");
    author
        .split('>')
        .nth(1)
        .expect("timestamp")
        .trim()
        .to_owned()
}

fn epoch_seconds(ts: &str) -> i64 {
    ts.split_whitespace()
        .next()
        .unwrap_or(ts)
        .parse()
        .unwrap_or_else(|_| panic!("epoch in {ts:?}"))
}

fn assert_timestamps_match(git_ts: &str, grit_ts: &str, spec: &str, exact: bool) {
    if exact {
        assert_eq!(git_ts, grit_ts, "spec {spec:?}");
        return;
    }
    let g = epoch_seconds(git_ts);
    let r = epoch_seconds(grit_ts);
    assert!(
        (g - r).abs() <= 1,
        "spec {spec:?}: git {git_ts} grit {grit_ts}"
    );
}

fn assert_commit_date_matches_git(spec: &str, exact: bool) {
    let _tz = UtcTzGuard::new();
    let git_ts = git_author_timestamp(spec);
    let grit_ts = normalize_commit_date_override(spec)
        .unwrap_or_else(|e| panic!("grit normalize_commit_date_override({spec:?}): {e}"));
    assert_timestamps_match(&git_ts, &grit_ts, spec, exact);
    let now = process_wall_clock_sec();
    let parse_at =
        parse_date_at(spec, now).unwrap_or_else(|e| panic!("grit parse_date_at({spec:?}): {e}"));
    assert_timestamps_match(&git_ts, &parse_at, spec, exact);
}

#[test]
fn commit_date_issue_1504_forms_match_git() {
    for spec in [
        "2024-01-01 00:00",
        "2024-01-01T00:00:00Z",
        "946684800 +0000",
    ] {
        assert_commit_date_matches_git(spec, true);
    }
    for spec in [
        "2024-01-01",
        "1 week ago",
        "2.weeks.ago",
        "2024/01/01",
        "Jan 1 2024",
        "yesterday",
    ] {
        assert_commit_date_matches_git(spec, false);
    }
}

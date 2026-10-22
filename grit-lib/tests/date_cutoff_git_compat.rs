//! `parse_date_cutoff` vs `git rev-parse --since=<spec>` (`--max-age=<ts>`).

use std::process::Command;
use std::sync::{LazyLock, Mutex, MutexGuard};

use grit_lib::config::git_date::parse_date_cutoff;
use grit_lib::config::git_date::tm::refresh_process_tz;

const NOW: i64 = 1_700_100_000;

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

fn git_since_cutoff_inherit_tz(spec: &str, now: i64) -> Option<i64> {
    let out = Command::new("git")
        .env("GIT_TEST_DATE_NOW", now.to_string())
        .args(["rev-parse", &format!("--since={spec}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim().strip_prefix("--max-age=")?.parse().ok()
}

fn assert_cutoff_matches_git(spec: &str, now: i64) {
    let _tz = UtcTzGuard::new();
    let grit = parse_date_cutoff(spec, now).unwrap_or_else(|e| panic!("grit {spec:?}: {e}"));
    let git = git_since_cutoff_inherit_tz(spec, now).unwrap_or_else(|| panic!("git {spec}"));
    assert_eq!(grit, git, "spec {spec:?}");
}

#[test]
fn cutoff_issue_1504_date_only_relative_and_fuzzy() {
    for spec in [
        "2024-01-01",
        "2020-01-01",
        "1 week ago",
        "2.weeks.ago",
        "2024/01/01",
        "Jan 1 2024",
        "yesterday",
    ] {
        assert_cutoff_matches_git(spec, NOW);
    }
}

#[test]
fn cutoff_relative_units() {
    for spec in [
        "1.second.ago",
        "2.minutes.ago",
        "3.hours.ago",
        "1.day.ago",
        "2.weeks.ago",
        "1.month.ago",
        "1.year.ago",
    ] {
        assert_cutoff_matches_git(spec, NOW);
    }
}

#[test]
fn cutoff_iso_and_rfc2822() {
    for spec in [
        "2024-01-15T12:00:00Z",
        "2024-01-15 12:00:00 +0000",
        "Thu, 15 Jan 2024 12:00:00 +0000",
    ] {
        assert_cutoff_matches_git(spec, NOW);
    }
    assert_cutoff_matches_git("yesterday", NOW);
    assert_cutoff_matches_git("noon", NOW);
}

#[test]
fn cutoff_epoch_at_form() {
    assert_cutoff_matches_git("@1700000000", NOW);
    let got = parse_date_cutoff("@1700000000", NOW).expect("@epoch");
    assert_eq!(got, 1_700_000_000);
}

#[test]
fn date_cutoff_matches_git_rev_parse_since_table() {
    let specs = [
        "1.second.ago",
        "5.minutes.ago",
        "2.hours.ago",
        "1.day.ago",
        "3.weeks.ago",
        "yesterday",
        "noon",
        "@1700000000",
        "2024-06-01T00:00:00Z",
        "Sat, 01 Jun 2024 00:00:00 +0000",
    ];
    for spec in specs {
        assert_cutoff_matches_git(spec, NOW);
    }
}

#[test]
fn date_cutoff_absolute_epoch_without_test_clock() {
    let _tz = UtcTzGuard::new();
    let spec = "@1609459200";
    let now = 1_700_100_000_i64;
    let git = git_since_cutoff_inherit_tz(spec, now).expect("git @epoch");
    let grit = parse_date_cutoff(spec, now).expect("grit @epoch");
    assert_eq!(grit, git);
    assert_eq!(grit, 1_609_459_200);
}

/// Regression: grit resolves `noon` in the process timezone; pinning only the git child to
/// UTC while grit runs under another zone makes the oracle lie (Bugbot #1253).
#[test]
fn cutoff_oracle_aligns_process_tz_with_git() {
    let _lock = TZ_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved = std::env::var("TZ").ok();
    std::env::set_var("TZ", "America/Los_Angeles");
    refresh_process_tz();
    let grit = parse_date_cutoff("noon", NOW).expect("grit noon");
    let git_utc_child_only = {
        let out = Command::new("git")
            .env("GIT_TEST_DATE_NOW", NOW.to_string())
            .env("TZ", "UTC")
            .args(["rev-parse", "--since=noon"])
            .output()
            .expect("git");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .strip_prefix("--max-age=")
            .expect("max-age")
            .parse::<i64>()
            .expect("integer")
    };
    assert_ne!(
        grit, git_utc_child_only,
        "grit under America/Los_Angeles must differ from git oracle with UTC pinned on child only"
    );
    std::env::set_var("TZ", "UTC");
    refresh_process_tz();
    let grit_utc = parse_date_cutoff("noon", NOW).expect("grit noon utc");
    let git_inherit = git_since_cutoff_inherit_tz("noon", NOW).expect("git noon inherit");
    assert_eq!(grit_utc, git_inherit);
    match saved {
        Some(v) => std::env::set_var("TZ", v),
        None => std::env::remove_var("TZ"),
    }
    refresh_process_tz();
}

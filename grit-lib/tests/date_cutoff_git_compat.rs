//! `parse_date_cutoff` vs `git rev-parse --since=<spec>` (`--max-age=<ts>`).

use std::process::Command;

use grit_lib::config::git_date::parse_date_cutoff;

const NOW: i64 = 1_700_100_000;

fn git_since_cutoff(spec: &str, now: i64) -> Option<i64> {
    let out = Command::new("git")
        .env("GIT_TEST_DATE_NOW", now.to_string())
        .env("TZ", "UTC")
        .args(["rev-parse", &format!("--since={spec}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim().strip_prefix("--max-age=")?.parse().ok()
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
        let git = git_since_cutoff(spec, NOW).unwrap_or_else(|| panic!("git failed for {spec:?}"));
        let grit = parse_date_cutoff(spec, NOW).unwrap_or_else(|e| panic!("grit {spec:?}: {e}"));
        assert_eq!(grit, git, "spec {spec:?}");
    }
}

#[test]
fn date_cutoff_absolute_epoch_without_test_clock() {
    let spec = "@1609459200";
    let now = 1_700_100_000_i64;
    let git = git_since_cutoff(spec, now).expect("git @epoch");
    let grit = parse_date_cutoff(spec, now).expect("grit @epoch");
    assert_eq!(grit, git);
    assert_eq!(grit, 1_609_459_200);
}

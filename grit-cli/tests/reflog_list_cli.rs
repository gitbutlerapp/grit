//! `grit reflog list` and `reflog expire --dry-run` JSON.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::CliFixture;
use serde_json::Value;

fn assert_ok(out: &grit_test_support::Output, label: &str) {
    assert!(
        out.ok(),
        "grit {label} failed: exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
}

fn grit_json(fixture: &CliFixture, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend(args);
    let out = fixture.run(&full);
    assert_ok(&out, "json");
    serde_json::from_str(&out.stdout).expect("json")
}

#[test]
fn reflog_list_cli() {
    let fixture = CliFixture::new();
    assert_ok(&fixture.run(&["branch", "list"]), "create list branch");
    assert_ok(&fixture.run(&["switch", "main"]), "switch main");

    let listed = grit_json(&fixture, &["reflog", "list"]);
    let refs = listed["refs"]
        .as_array()
        .expect("refs array")
        .iter()
        .filter_map(|v| v.get("ref").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        refs.iter().any(|r| *r == "refs/heads/main"),
        "expected main reflog: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| *r == "refs/heads/list"),
        "branch named list must appear as refs/heads/list: {refs:?}"
    );

    let show_list = fixture.run(&["reflog", "refs/heads/list", "--limit", "1"]);
    assert_ok(&show_list, "show list branch");
    assert!(
        show_list.stdout.contains("list@"),
        "full ref shows entries for branch named list"
    );

    let dry = grit_json(
        &fixture,
        &[
            "reflog",
            "expire",
            "--dry-run",
            "--expire",
            "never",
            "--expire-unreachable",
            "never",
        ],
    );
    assert_eq!(dry["dry_run"], true);
    assert!(dry["refs"].is_array());
    assert!(dry["entries_pruned"].is_number());
}

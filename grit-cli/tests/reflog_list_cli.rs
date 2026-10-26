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

#[test]
fn invalid_reflog_index_keeps_the_parse_explanation_in_every_output_mode() {
    let fixture = CliFixture::new();
    let message = "invalid reflog index in 'HEAD@{bad}': invalid digit found in string";
    for mode in [None, Some("--json"), Some("--markdown")] {
        let mut args = vec!["reflog", "delete", "HEAD@{bad}"];
        args.extend(mode);
        let out = fixture.run(&args);
        assert!(!out.ok());
        match mode {
            Some("--json") => {
                let value: Value = serde_json::from_str(&out.stdout).expect("JSON error");
                assert_eq!(value["error"], message);
                assert!(out.stderr.is_empty());
            }
            Some("--markdown") => {
                assert_eq!(out.stdout, format!("**Error:** {message}\n"));
                assert!(out.stderr.is_empty());
            }
            _ => {
                assert_eq!(out.stderr, format!("error: {message}\n"));
                assert!(out.stdout.is_empty());
            }
        }
    }
}

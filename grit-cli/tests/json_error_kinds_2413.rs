//! `--json` errors expose stable `kind` for common CI failures (issue #2413).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::CliFixture;
use serde_json::Value;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn json_err(fixture: &CliFixture, args: &[&str]) -> Value {
    let out = fixture.run(args);
    assert_eq!(out.status, Some(1), "{}", out.dump("json error"));
    serde_json::from_str(out.stdout.trim()).expect("json on stdout")
}

#[test]
fn json_error_kinds_for_ci_failures() {
    let no_repo = CliFixture::workspace();
    let err = json_err(&no_repo, &["--json", "status"]);
    assert_eq!(err["kind"], "not_a_repository");
    assert!(err["error"]
        .as_str()
        .unwrap_or("")
        .contains("not a git repository"));

    let fixture = CliFixture::empty();
    std::fs::write(fixture.path().join("a"), "a\n").unwrap();
    fixture.run_ok(&["commit", "-m", "one"]);

    assert_eq!(
        json_err(&fixture, &["--json", "commit"])["kind"],
        "commit_message_required"
    );
    assert_eq!(
        json_err(&fixture, &["--json", "commit", "-m", ""])["kind"],
        "empty_commit_message"
    );

    let identity = Command::new(GRIT)
        .current_dir(fixture.path())
        .args(["--json", "commit", "-m", "x"])
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert_eq!(identity.status.code(), Some(1));
    let err: Value = serde_json::from_str(String::from_utf8_lossy(&identity.stdout).trim())
        .expect("identity json");
    assert_eq!(err["kind"], "identity_unknown");
    assert!(err.get("hint").is_some());
    assert!(!err["error"].as_str().unwrap_or("").contains("grit config"));

    fixture.run_ok(&["tag", "v1"]);
    assert_eq!(
        json_err(&fixture, &["--json", "tag", "v1"])["kind"],
        "tag_already_exists"
    );

    for args in [
        &["--json", "merge", "--abort"][..],
        &["--json", "rebase", "--continue"][..],
        &["--json", "pick", "--continue"][..],
    ] {
        let err = json_err(&fixture, args);
        assert_eq!(err["kind"], "no_operation_in_progress", "args={args:?}");
    }

    for args in [
        &["--json", "add", "nosuch"][..],
        &["--json", "rm", "nosuch"][..],
        &["--json", "restore", "nosuch"][..],
    ] {
        let err = json_err(&fixture, args);
        assert_eq!(err["kind"], "pathspec_no_match", "args={args:?}");
    }

    assert_eq!(
        json_err(&fixture, &["--json", "stash", "pop"])["kind"],
        "stash_not_found"
    );
    assert_eq!(
        json_err(&fixture, &["--json", "blame", "nosuchfile"])["kind"],
        "path_not_found"
    );
}

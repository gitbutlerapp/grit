//! Default integration tests for `grit push --force-if-includes` and config.

use grit_test_support::{git, grit_cmd, CliFixture};
use std::fs;

fn fetch_amend_unintegrated(client: &std::path::Path, bare: &std::path::Path) {
    git(client, &["push", "-q", "origin", "main"]);
    git(client, &["fetch", "-q", "origin"]);
    let stale = git(bare, &["rev-parse", "refs/heads/main"])
        .trim()
        .to_string();
    let tree = git(bare, &["rev-parse", &format!("{stale}^{{tree}}")])
        .trim()
        .to_string();
    let other = git(bare, &["commit-tree", &tree, "-p", &stale, "-m", "other"])
        .trim()
        .to_string();
    git(bare, &["update-ref", "refs/heads/main", &other]);
    git(client, &["fetch", "-q", "origin"]);
    fs::write(client.join("extra.txt"), "x\n").unwrap();
    git(client, &["add", "extra.txt"]);
    git(client, &["commit", "-q", "--amend", "--no-edit"]);
}

#[test]
fn config_use_force_if_includes_rejects_when_unintegrated() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    fetch_amend_unintegrated(&client, bare);
    git(&client, &["config", "push.useForceIfIncludes", "true"]);
    let out = grit_cmd(&["push", "--force-with-lease", "--json"])
        .in_dir(&client)
        .exec();
    assert!(!out.ok(), "{}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert!(
        value["results"]
            .as_array()
            .is_some_and(|r| r.iter().any(|x| x["reason"] == "remote_updated")),
        "{value}"
    );
}

#[test]
fn no_force_if_includes_overrides_config() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    fetch_amend_unintegrated(&client, bare);
    git(&client, &["config", "push.useForceIfIncludes", "true"]);
    let out = grit_cmd(&[
        "push",
        "--force-with-lease",
        "--no-force-if-includes",
        "--json",
    ])
    .in_dir(&client)
    .exec();
    assert!(
        out.ok(),
        "without includes check, lease should pass: {}",
        out.stderr
    );
}

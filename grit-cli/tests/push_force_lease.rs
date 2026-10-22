//! `grit push --force`, `--force-with-lease`, and JSON stale rejection.

use grit_test_support::{git, grit_cmd, CliFixture};

#[test]
fn force_and_force_with_lease_conflict() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");
    let out = grit_cmd(&["push", "--force", "--force-with-lease"])
        .in_dir(&client)
        .exec();
    assert!(!out.ok(), "conflicting flags should fail");
    assert!(
        out.stderr.contains("force-with-lease") || out.stderr.contains("force"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn force_with_lease_stale_json_reason() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(client.join("t.txt"), "1\n").unwrap();
    git(&client, &["add", "t.txt"]);
    git(&client, &["commit", "-q", "-m", "topic"]);
    git(&client, &["push", "-q", "origin", "topic"]);
    git(&client, &["fetch", "-q", "origin"]);
    let on_remote = git(bare, &["rev-parse", "refs/heads/topic"])
        .trim()
        .to_string();
    std::fs::write(client.join("t2.txt"), "2\n").unwrap();
    git(&client, &["add", "t2.txt"]);
    git(&client, &["commit", "-q", "--amend", "--no-edit"]);
    let tree = git(
        bare,
        &["rev-parse", &format!("{on_remote}^{{tree}}")],
    )
    .trim()
    .to_string();
    let other = git(
        bare,
        &[
            "commit-tree",
            &tree,
            "-p",
            &on_remote,
            "-m",
            "other",
        ],
    );
    git(
        bare,
        &["update-ref", "refs/heads/topic", other.trim()],
    );
    let out = grit_cmd(&["push", "--force-with-lease", "--json", "origin", "topic"])
        .in_dir(&client)
        .exec();
    assert!(!out.ok(), "stale lease should exit 1: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], true);
    let results = value["results"].as_array().expect("results");
    assert!(
        results
            .iter()
            .any(|r| { r["status"] == "rejected" && r["reason"] == "stale" }),
        "payload: {value}"
    );
}

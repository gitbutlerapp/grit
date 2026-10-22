//! JSON output for `grit push` flags added in the push options wave.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::{git, grit_cmd, CliFixture};
use serde_json::Value;

#[test]
fn push_json_set_upstream_atomic_follow_tags_and_push_option() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(client.join("t.txt"), "1\n").unwrap();
    git(&client, &["add", "t.txt"]);
    git(&client, &["commit", "-q", "-m", "topic commit"]);
    git(&client, &["tag", "-a", "-m", "v", "v-topic"]);

    let push = grit_cmd(&["push", "-u", "--follow-tags", "origin", "topic", "--json"])
        .in_dir(&client)
        .exec();
    assert!(push.ok(), "push failed: {}", push.stderr);
    let json: Value = serde_json::from_str(&push.stdout).expect("json");
    assert_eq!(json["upstream_set"], serde_json::json!(["topic"]));
    assert_eq!(json["rejected"], Value::Bool(false));
    let refs: Vec<_> = json["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| r["ref"].as_str().unwrap().to_owned())
        .collect();
    assert!(refs.iter().any(|r| r == "refs/heads/topic"));
    assert!(refs.iter().any(|r| r == "refs/tags/v-topic"));

    git(&client, &["config", "--unset", "branch.topic.remote"]);
    git(&client, &["config", "--unset", "branch.topic.merge"]);

    let dry = grit_cmd(&["push", "-u", "--dry-run", "origin", "topic", "--json"])
        .in_dir(&client)
        .exec();
    assert!(dry.ok(), "dry-run push: {}", dry.stderr);
    let dry_json: Value = serde_json::from_str(&dry.stdout).expect("dry json");
    assert!(
        dry_json.get("upstream_set").is_none()
            || dry_json["upstream_set"].as_null().is_some()
            || dry_json["upstream_set"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
        "dry-run must not set upstream"
    );

    let atomic_dry = grit_cmd(&["push", "--atomic", "--dry-run", "origin", "topic", "--json"])
        .in_dir(&client)
        .exec();
    assert!(atomic_dry.ok(), "atomic dry-run: {}", atomic_dry.stderr);
    let atomic_json: Value = serde_json::from_str(&atomic_dry.stdout).expect("atomic json");
    assert_eq!(atomic_json["dry_run"], Value::Bool(true));

    let unsupported = grit_cmd(&["push", "-o", "ci.skip", "origin", "topic", "--json"])
        .in_dir(&client)
        .exec();
    assert!(
        !unsupported.ok(),
        "file:// push without push-options capability should fail"
    );
    let err_text = format!("{}{}", unsupported.stderr, unsupported.stdout);
    assert!(
        err_text.contains("push-options") || err_text.contains("push options"),
        "expected push-options error: {err_text}"
    );
}

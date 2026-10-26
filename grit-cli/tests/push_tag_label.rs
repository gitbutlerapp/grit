//! Tag pushes must name the tag in human output, not the current branch (#2004).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::{git, grit_cmd, CliFixture};
use serde_json::Value;

#[test]
fn push_single_tag_summary_and_json_branch_field() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");
    git(&client, &["tag", "-a", "-m", "ann", "v1"]);
    git(&client, &["tag", "v2"]);

    let human = grit_cmd(&["push", "origin", "v1"]).in_dir(&client).exec();
    assert!(human.ok(), "push v1: {}", human.stderr);
    assert!(
        human.stdout.contains("pushed v1 → origin refs/tags/v1"),
        "expected tag name in summary, got: {}",
        human.stdout
    );
    assert!(
        !human.stdout.contains("pushed main →"),
        "must not label tag push as current branch: {}",
        human.stdout
    );

    let json = grit_cmd(&["push", "origin", "v2", "--json"])
        .in_dir(&client)
        .exec();
    assert!(json.ok(), "push v2 json: {}", json.stderr);
    let parsed: Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(parsed["kind"], "refspecs");
    assert!(
        parsed.get("branch").is_none() || parsed["branch"].is_null(),
        "tag push must omit branch field, got: {}",
        parsed["branch"]
    );
    assert_eq!(parsed["results"][0]["ref"], "refs/tags/v2");
}

//! Push human output names each source ref; follow-tags JSON uses `refspecs` (#2004, #2398).

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

#[test]
fn push_multiple_branches_names_each_source() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "b2"]);
    std::fs::write(client.join("b2.txt"), "b2\n").unwrap();
    git(&client, &["add", "b2.txt"]);
    git(&client, &["commit", "-qm", "on b2"]);
    git(&client, &["checkout", "-q", "main"]);
    std::fs::write(client.join("main.txt"), "main\n").unwrap();
    git(&client, &["add", "main.txt"]);
    git(&client, &["commit", "-qm", "on main"]);

    let human = grit_cmd(&["push", "-u", "origin", "main", "b2"])
        .in_dir(&client)
        .exec();
    assert!(human.ok(), "multi push: {}", human.stderr);
    assert!(
        human
            .stdout
            .contains("pushed main → origin refs/heads/main"),
        "expected main label, got: {}",
        human.stdout
    );
    assert!(
        human.stdout.contains("pushed b2 → origin refs/heads/b2"),
        "expected b2 label, got: {}",
        human.stdout
    );
    assert!(
        !human.stdout.contains("pushed refs →"),
        "must not use generic refs label: {}",
        human.stdout
    );
}

#[test]
fn push_refspec_to_different_remote_branch_names_source() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");

    let human = grit_cmd(&["push", "origin", "main:main2"])
        .in_dir(&client)
        .exec();
    assert!(human.ok(), "push main:main2: {}", human.stderr);
    assert!(
        human
            .stdout
            .contains("pushed main → origin refs/heads/main2"),
        "expected source branch main, got: {}",
        human.stdout
    );
}

#[test]
fn push_follow_tags_names_each_tag_and_json_kind_is_refspecs() {
    let fx = CliFixture::new().with_upstream();
    let client = fx.git_clone_client("client");
    git(&client, &["tag", "-a", "-m", "v1", "v1.0"]);
    std::fs::write(client.join("tagged.txt"), "tag\n").unwrap();
    git(&client, &["add", "tagged.txt"]);
    git(&client, &["commit", "-qm", "after v1"]);
    git(&client, &["tag", "-a", "-m", "v2", "v1.1"]);

    let human = grit_cmd(&["push", "--follow-tags", "origin", "main"])
        .in_dir(&client)
        .exec();
    assert!(human.ok(), "follow-tags push: {}", human.stderr);
    assert!(
        human
            .stdout
            .contains("pushed main → origin refs/heads/main"),
        "branch line: {}",
        human.stdout
    );
    assert!(
        human.stdout.contains("pushed v1.1 → origin refs/tags/v1.1"),
        "tag line must name the tag, got: {}",
        human.stdout
    );
    assert!(
        !human.stdout.contains("pushed main → origin refs/tags/"),
        "tag must not be labeled as main: {}",
        human.stdout
    );

    let json = grit_cmd(&["push", "--follow-tags", "origin", "main", "--json"])
        .in_dir(&client)
        .exec();
    assert!(json.ok(), "follow-tags json: {}", json.stderr);
    let parsed: Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(parsed["kind"], "refspecs");
    assert!(
        parsed.get("branch").is_none() || parsed["branch"].is_null(),
        "branch+tags push must omit branch field, got: {}",
        parsed["branch"]
    );
}

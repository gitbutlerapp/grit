//! `grit push` rejects updating an existing remote tag without `--force`.

use grit_test_support::{git, grit_cmd, CliFixture};

#[test]
fn push_existing_tag_exits_one_and_json_tag_exists() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    std::fs::write(client.join("f"), b"1\n2\n").unwrap();
    git(&client, &["commit", "-q", "-am", "c2"]);
    git(&client, &["tag", "v1", "HEAD~1"]);
    git(&client, &["push", "-q", "origin", "main", "v1"]);
    let on_remote = git(bare, &["rev-parse", "refs/tags/v1"]).trim().to_string();
    git(&client, &["tag", "-f", "v1", "HEAD"]);
    let out = grit_cmd(&["push", "--json", "origin", "v1"])
        .in_dir(&client)
        .exec();
    assert!(
        !out.ok(),
        "tag move without force should fail: {}",
        out.stderr
    );
    assert_eq!(
        git(bare, &["rev-parse", "refs/tags/v1"]).trim(),
        on_remote,
        "remote tag unchanged"
    );
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], true);
    let results = value["results"].as_array().expect("results");
    assert!(
        results
            .iter()
            .any(|r| { r["status"] == "rejected" && r["reason"] == "tag_exists" }),
        "{value}"
    );
}

#[test]
fn push_tags_rejects_existing_while_new_tags_still_push() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    git(&client, &["tag", "old", "HEAD"]);
    git(&client, &["push", "-q", "origin", "old"]);
    std::fs::write(client.join("g"), b"g\n").unwrap();
    git(&client, &["add", "g"]);
    git(&client, &["commit", "-q", "-m", "c2"]);
    git(&client, &["tag", "-f", "old", "HEAD"]);
    git(&client, &["tag", "new", "HEAD"]);
    let kept = git(bare, &["rev-parse", "refs/tags/old"])
        .trim()
        .to_string();
    let out = grit_cmd(&["push", "--tags", "--json", "origin"])
        .in_dir(&client)
        .exec();
    assert!(!out.ok(), "partial tag rejection should exit 1");
    assert_eq!(git(bare, &["rev-parse", "refs/tags/old"]).trim(), kept);
    assert!(!git(bare, &["rev-parse", "refs/tags/new"]).trim().is_empty());
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], true);
}

#[test]
fn push_force_replaces_existing_tag() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    std::fs::write(client.join("f"), b"1\n").unwrap();
    git(&client, &["add", "f"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    git(&client, &["tag", "v1", "HEAD"]);
    git(&client, &["push", "-q", "origin", "v1"]);
    std::fs::write(client.join("f"), b"1\n2\n").unwrap();
    git(&client, &["commit", "-q", "-am", "c2"]);
    git(&client, &["tag", "-f", "v1", "HEAD"]);
    let want = git(&client, &["rev-parse", "refs/tags/v1"])
        .trim()
        .to_string();
    let out = grit_cmd(&["push", "--force", "origin", "v1"])
        .in_dir(&client)
        .exec();
    assert!(out.ok(), "force tag update: {}", out.stderr);
    assert_eq!(git(bare, &["rev-parse", "refs/tags/v1"]).trim(), want);
}

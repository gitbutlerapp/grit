//! `grit push --dry-run` leaves the bare remote unchanged.

use grit_test_support::{git, grit_cmd, CliFixture};

#[test]
fn dry_run_does_not_update_bare_remote() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(client.join("dry.txt"), "x\n").unwrap();
    git(&client, &["add", "dry.txt"]);
    git(&client, &["commit", "-q", "-m", "dry-run topic"]);
    let before_refs = git(bare, &["for-each-ref", "--format=%(refname)"]);
    let before_objects = git(bare, &["rev-list", "--all", "--count"]);
    let out = grit_cmd(&["push", "--dry-run", "origin", "topic"])
        .in_dir(&client)
        .exec();
    assert!(out.ok(), "grit push --dry-run failed: {}", out.stderr);
    let after_refs = git(bare, &["for-each-ref", "--format=%(refname)"]);
    let after_objects = git(bare, &["rev-list", "--all", "--count"]);
    assert_eq!(before_refs, after_refs, "bare refs changed after dry-run");
    assert_eq!(
        before_objects, after_objects,
        "bare objects changed after dry-run"
    );
}

#[test]
fn bare_path_remote_positional_not_parsed_as_refspec() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(client.join("path-remote.txt"), "x\n").unwrap();
    git(&client, &["add", "path-remote.txt"]);
    git(&client, &["commit", "-q", "-m", "path remote"]);
    let bare_str = bare.to_str().expect("bare path utf8");
    let out = grit_cmd(&["push", "--dry-run", bare_str, "topic"])
        .in_dir(&client)
        .exec();
    assert!(
        out.ok(),
        "grit push <bare-path> <ref> should treat path as remote: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("would push") || out.stdout.contains("up to date"),
        "expected push plan output, got: {}",
        out.stdout
    );
}

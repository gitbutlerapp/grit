//! `grit push --dry-run` leaves the bare remote unchanged.

use grit_test_support::{git, git_cmd, CliFixture};

#[test]
fn dry_run_does_not_update_bare_remote() {
    let fx = CliFixture::new().with_upstream();
    let bare = fx.upstream_path();
    let client = fx.git_clone_client("client");
    git(&client, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(client.join("dry.txt"), "x\n").unwrap();
    git(&client, &["add", "dry.txt"]);
    git(&client, &["commit", "-q", "-m", "dry-run topic"]);
    let before = git(bare, &["for-each-ref", "--format=%(refname)"]);
    let out = git_cmd(&["push", "--dry-run", "origin", "topic"])
        .in_dir(&client)
        .exec();
    assert!(out.ok(), "grit push --dry-run failed: {}", out.stderr);
    let after = git(bare, &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(before, after, "bare refs changed after dry-run");
}

//! `grit push` glob refspecs and `--all` against a new bare remote.

use grit_test_support::{git, grit_cmd, CliFixture};

#[test]
fn push_glob_refspec_publishes_all_branches() {
    let fx = CliFixture::new();
    let root = fx.path().parent().unwrap();
    let bare = root.join("new.git");
    std::fs::create_dir_all(&bare).expect("bare dir");
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(fx.path(), &["branch", "b1"]);
    git(fx.path(), &["branch", "b2"]);
    let bare_str = bare.to_str().expect("bare utf-8");
    git(fx.path(), &["remote", "add", "nb", bare_str]);
    let out = grit_cmd(&["push", "nb", "refs/heads/*:refs/heads/*"])
        .in_dir(fx.path())
        .exec();
    assert!(out.ok(), "grit push glob refspec failed: {}", out.stderr);
    let refs = git(&bare, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/heads/main"));
    assert!(refs.contains("refs/heads/b1"));
    assert!(refs.contains("refs/heads/b2"));
    let fsck = git(&bare, &["fsck", "--strict"]);
    assert!(fsck.is_empty() || !fsck.contains("error"), "fsck: {fsck}");
}

#[test]
fn push_all_publishes_all_branches() {
    let fx = CliFixture::new();
    let root = fx.path().parent().unwrap();
    let bare = root.join("all.git");
    std::fs::create_dir_all(&bare).expect("bare dir");
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(fx.path(), &["branch", "side"]);
    let bare_str = bare.to_str().expect("bare utf-8");
    git(fx.path(), &["remote", "add", "nb", bare_str]);
    let out = grit_cmd(&["push", "nb", "--all"]).in_dir(fx.path()).exec();
    assert!(out.ok(), "grit push --all failed: {}", out.stderr);
    let refs = git(&bare, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/heads/main"));
    assert!(refs.contains("refs/heads/feature"));
    assert!(refs.contains("refs/heads/side"));
}

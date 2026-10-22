//! Smoke tests for `grit diff` index/worktree paths (scan counting lives in `grit-lib` unit tests).

use grit_test_support::cli_fixture::CliFixture;
use grit_test_support::{git, grit};

#[test]
fn diff_default_patch_uses_grit_binary() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"base\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"dirty\n").expect("write");
    let out = grit(fx.path(), &["diff"]);
    assert!(out.ok(), "{}", out.dump("default diff"));
}

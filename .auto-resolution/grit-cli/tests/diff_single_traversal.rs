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

#[test]
fn diff_staged_reads_the_index_once() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"base\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"staged\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);

    for args in [
        &["diff", "--staged"][..],
        &["diff", "--staged", "--stat"],
        &["diff", "--staged", "--patch-text"],
    ] {
        let out = grit_test_support::grit_cmd(args)
            .in_dir(fx.path())
            .env("GRIT_TRACE", "1")
            .exec();
        assert!(out.ok(), "{}", out.dump("staged diff"));
        let index_reads = out.stderr.matches("grit.index.read: close").count();
        assert_eq!(index_reads, 1, "{args:?} trace:\n{}", out.stderr);
    }
}

//! Regression for issue #1876: `grit gc midx` must leave the repository readable by the
//! system `git` (multi-pack-index v1 unless `midx.version` asks for v2).

use grit_test_support::{git, CliFixture};

fn midx_header_version(fixture: &CliFixture) -> u8 {
    let path = fixture.path().join(".git/objects/pack/multi-pack-index");
    std::fs::read(&path).expect("read multi-pack-index")[4]
}

#[test]
fn gc_midx_writes_v1_and_git_keeps_working() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["gc", "repack"]);
    fixture.run_ok(&["gc", "midx"]);
    assert_eq!(midx_header_version(&fixture), 1);
    let root = fixture.path();
    git(root, &["log", "--oneline", "-1"]);
    git(root, &["status", "--short"]);
    git(root, &["fsck"]);
    git(root, &["multi-pack-index", "verify"]);
}

#[test]
fn gc_midx_honours_midx_version_config() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["gc", "repack"]);
    let root = fixture.path();

    git(root, &["config", "midx.version", "2"]);
    fixture.run_ok(&["gc", "midx"]);
    assert_eq!(midx_header_version(&fixture), 2);

    git(root, &["config", "midx.version", "1"]);
    fixture.run_ok(&["gc", "midx"]);
    assert_eq!(midx_header_version(&fixture), 1);
    git(root, &["multi-pack-index", "verify"]);

    git(root, &["config", "midx.version", "7"]);
    let out = fixture.run(&["gc", "midx", "--json"]);
    assert_ne!(
        out.status,
        Some(0),
        "midx.version=7 must fail: {}",
        out.stdout
    );
    assert_eq!(
        midx_header_version(&fixture),
        1,
        "a rejected write leaves the old file alone"
    );
}

//! `grit init --ref-format reftable` produces a repo system Git can read.

use std::process::Command;
use std::sync::OnceLock;

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git_supports_reftable() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q", "--ref-format=reftable"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
fn grit_init_reftable_git_for_each_ref_and_fsck() {
    if !git_supports_reftable() {
        if std::env::var("GRIT_REQUIRE_REFTABLE_GIT")
            .ok()
            .is_some_and(|v| v == "1")
        {
            panic!("required reftable-capable system git (>= 2.45)");
        }
        eprintln!("SKIP: git lacks reftable");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    let grit = env!("CARGO_BIN_EXE_grit");
    let status = Command::new(grit)
        .args(["init", "--ref-format", "reftable"])
        .arg(&root)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("grit init");
    assert!(status.success(), "grit init --ref-format reftable failed");

    let git_dir = root.join(".git");
    let for_each = Command::new("git")
        .current_dir(&root)
        .args(["for-each-ref"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git for-each-ref");
    assert!(
        for_each.status.success(),
        "git for-each-ref: {}",
        String::from_utf8_lossy(&for_each.stderr)
    );

    let fsck = Command::new("git")
        .args(["-C", git_dir.to_str().unwrap(), "fsck"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

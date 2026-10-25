//! `grit repack`, `grit gc` and `grit fsck` succeed in shallow clones (issue #1882).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git")
}

fn grit(dir: &Path, args: &[&str]) -> Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit")
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn repack_gc_and_fsck_succeed_in_depth_clones() {
    let base = tempfile::tempdir().expect("tempdir");
    let src = base.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    assert_ok(&git(&src, &["init", "-q", "-b", "main"]), "git init");
    for i in 1..=4 {
        std::fs::write(src.join("f"), format!("{i}\n")).unwrap();
        assert_ok(&git(&src, &["add", "f"]), "git add");
        assert_ok(
            &git(&src, &["commit", "-q", "-m", &format!("c{i}")]),
            "git commit",
        );
    }
    let url = format!("file://{}", src.display());

    let git_clone = base.path().join("git-clone");
    assert_ok(
        &git(
            base.path(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &url,
                git_clone.to_str().unwrap(),
            ],
        ),
        "git clone --depth 1",
    );
    let grit_clone = base.path().join("grit-clone");
    assert_ok(
        &grit(
            base.path(),
            &["clone", "--depth", "2", &url, grit_clone.to_str().unwrap()],
        ),
        "grit clone --depth 2",
    );

    for clone in [&git_clone, &grit_clone] {
        assert!(clone.join(".git/shallow").is_file());

        let fsck = grit(clone, &["fsck", "--json"]);
        assert_ok(&fsck, "grit fsck --json");
        let report: serde_json::Value = serde_json::from_slice(&fsck.stdout).expect("fsck json");
        assert_eq!(report["ok"], serde_json::Value::Bool(true), "{report}");

        let repack = grit(clone, &["repack", "--json"]);
        assert_ok(&repack, "grit repack --json");
        let report: serde_json::Value =
            serde_json::from_slice(&repack.stdout).expect("repack json");
        assert!(report.get("error").is_none(), "{report}");

        assert_ok(&grit(clone, &["gc"]), "grit gc");
        assert_ok(&grit(clone, &["fsck"]), "grit fsck");
        assert_ok(&git(clone, &["fsck", "--full"]), "git fsck --full");
    }
}

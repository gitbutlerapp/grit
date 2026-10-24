//! `grit commit` from a subdirectory runs hooks from a relative `core.hooksPath`
//! (issue #1799).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use grit_test_support::{git, grit_cmd, CliFixture};

#[test]
fn commit_from_subdirectory_runs_relative_hooks_path_hook() {
    let fx = CliFixture::new();
    let repo = fx.path();
    let hook = repo.join("tools/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().expect("hook dir")).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nexit 5\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(repo, &["config", "core.hooksPath", "tools/hooks"]);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/file.txt"), "x\n").unwrap();
    let before = git(repo, &["rev-parse", "HEAD"]);

    let out = grit_cmd(&["commit", "-m", "rel"])
        .in_dir(&repo.join("sub"))
        .exec();
    assert_eq!(out.status, Some(1), "{}", out.dump("commit from sub/"));
    assert!(
        out.stderr
            .contains("hook pre-commit failed with exit status 5"),
        "{}",
        out.stderr
    );
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), before, "no commit made");
}

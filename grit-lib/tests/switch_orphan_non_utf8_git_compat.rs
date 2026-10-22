//! `grit switch --orphan` with a tracked non-UTF-8 path (Unix).

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::switch::{resolve_orphan_branch, switch_repository, SwitchOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

const BAD: &[u8] = b"bad-\xff";

#[test]
fn switch_orphan_non_utf8_tracked_path_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir.path())
        .status()
        .expect("init");
    for args in [
        ["config", "user.email", "a@b.c"],
        ["config", "user.name", "A"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("config");
    }

    std::fs::write(dir.path().join(OsStr::from_bytes(BAD)), b"x\n").expect("write");
    Command::new("git")
        .args(["add", "--", "."])
        .current_dir(dir.path())
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "c"])
        .current_dir(dir.path())
        .status()
        .expect("commit");

    Command::new("git")
        .args(["switch", "--orphan", "fresh"])
        .current_dir(dir.path())
        .status()
        .expect("git orphan");
    let git_status = Command::new("git")
        .args(["status", "--porcelain=v2", "-b"])
        .current_dir(dir.path())
        .output()
        .expect("git status");

    Command::new("git")
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .expect("back");
    std::fs::write(dir.path().join(OsStr::from_bytes(BAD)), b"x\n").expect("rewrite");
    Command::new("git")
        .args(["add", "--", "."])
        .current_dir(dir.path())
        .status()
        .expect("re-add");
    Command::new("git")
        .args(["update-index", "--refresh"])
        .current_dir(dir.path())
        .status()
        .expect("refresh index");

    let porcelain = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .expect("porcelain");
    assert!(
        porcelain.stdout.is_empty(),
        "git must see a clean tree before grit orphan: {:?}",
        String::from_utf8_lossy(&porcelain.stdout)
    );

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let mode = resolve_orphan_branch(&repo, "fresh").expect("orphan");
    switch_repository(&repo, &SwitchOptions::new(mode)).expect("grit orphan");

    let grit_status = Command::new("git")
        .args(["status", "--porcelain=v2", "-b"])
        .current_dir(dir.path())
        .output()
        .expect("grit status");
    assert_eq!(git_status.stdout, grit_status.stdout);
    assert!(!dir.path().join(OsStr::from_bytes(BAD)).exists());
}

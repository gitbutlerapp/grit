//! Clone an empty bare remote and compare layout with system `git`.

use std::process::Command;

use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn clone_empty_bare_matches_git_fsck_and_config() {
    let root = tempfile::tempdir().expect("tempdir");
    let bare = root.path().join("origin.git");
    git(
        root.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );

    let grit_dest = root.path().join("grit-clone");
    clone(
        CloneOptions::new(bare.to_string_lossy(), grit_dest.clone()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit clone");

    let git_dest = root.path().join("git-clone");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            git_dest.to_str().unwrap(),
        ],
    );

    for dest in [&grit_dest, &git_dest] {
        git(dest, &["fsck", "--strict"]);
    }

    let grit_head = std::fs::read_to_string(grit_dest.join(".git/HEAD")).expect("grit HEAD");
    let git_head = std::fs::read_to_string(git_dest.join(".git/HEAD")).expect("git HEAD");
    assert_eq!(grit_head.trim(), git_head.trim());

    for key in [
        "branch.main.remote",
        "branch.main.merge",
        "remote.origin.url",
    ] {
        let grit_val = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(&grit_dest)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git config grit");
        let git_val = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(&git_dest)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git config git");
        assert_eq!(
            String::from_utf8_lossy(&grit_val.stdout).trim(),
            String::from_utf8_lossy(&git_val.stdout).trim(),
            "{key}"
        );
    }

    assert!(
        !grit_dest.join(".git/refs/heads/main").exists(),
        "unborn branch must not create refs/heads/main"
    );
}

/// Bare remote advertises unborn `trunk` while `feature` has commits; clone must follow Git.
#[test]
fn clone_unborn_advertised_trunk_with_populated_feature_matches_git() {
    let root = tempfile::tempdir().expect("tempdir");
    let bare = root.path().join("origin.git");
    git(
        root.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "trunk",
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);

    let wt = root.path().join("wt");
    std::fs::create_dir_all(&wt).expect("mkdir wt");
    git(&wt, &["init", "-q"]);
    git(&wt, &["config", "user.email", "t@example.com"]);
    git(&wt, &["config", "user.name", "Test"]);
    std::fs::write(wt.join("file"), b"x\n").expect("write");
    git(&wt, &["add", "file"]);
    git(&wt, &["commit", "-m", "on feature"]);
    git(&wt, &["branch", "feature"]);
    git(&wt, &["push", bare.to_str().unwrap(), "feature"]);

    let grit_dest = root.path().join("grit-clone");
    clone(
        CloneOptions::new(bare.to_string_lossy(), grit_dest.clone()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit clone");

    let git_dest = root.path().join("git-clone");
    let git_out = Command::new("git")
        .args([
            "clone",
            "-q",
            bare.to_str().unwrap(),
            git_dest.to_str().unwrap(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git clone");
    assert!(
        git_out.status.success(),
        "{}",
        String::from_utf8_lossy(&git_out.stderr)
    );

    let grit_head = std::fs::read_to_string(grit_dest.join(".git/HEAD")).expect("grit HEAD");
    let git_head = std::fs::read_to_string(git_dest.join(".git/HEAD")).expect("git HEAD");
    assert_eq!(grit_head.trim(), git_head.trim());
    assert_eq!(grit_head.trim(), "ref: refs/heads/trunk");

    for dest in [&grit_dest, &git_dest] {
        let heads_dir = dest.join(".git/refs/heads");
        if heads_dir.is_dir() {
            let count = std::fs::read_dir(&heads_dir)
                .expect("read heads")
                .filter_map(|e| e.ok())
                .count();
            assert_eq!(
                count, 0,
                "no local branch tips for unborn advertised default"
            );
        }
    }

    for key in ["branch.trunk.remote", "branch.trunk.merge"] {
        let grit_val = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(&grit_dest)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git config grit");
        let git_val = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(&git_dest)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git config git");
        assert_eq!(
            String::from_utf8_lossy(&grit_val.stdout).trim(),
            String::from_utf8_lossy(&git_val.stdout).trim(),
            "{key}"
        );
    }
}

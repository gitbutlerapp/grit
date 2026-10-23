//! `git clone --bare` and `--no-checkout` compatibility.

use std::process::Command;

use grit_lib::remote::clone::{clone as clone_repo, CloneCheckout, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_ok(dir: &std::path::Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn sorted_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn config_keys(dir: &std::path::Path) -> Vec<String> {
    sorted_lines(&git_stdout(
        dir,
        &["config", "--list", "--local", "--name-only"],
    ))
}

fn for_each_ref(dir: &std::path::Path) -> Vec<String> {
    sorted_lines(&git_stdout(
        dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/"],
    ))
}

#[test]
fn clone_bare_matches_git_layout() {
    let upstream = tempfile::tempdir().expect("upstream");
    git_ok(upstream.path(), &["init", "-q", "-b", "main"]);
    git_ok(
        upstream.path(),
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "--allow-empty",
            "-qm",
            "c0",
        ],
    );
    git_ok(upstream.path(), &["branch", "feature"]);
    git_ok(upstream.path(), &["tag", "v1"]);

    let git_bare = tempfile::tempdir().expect("git bare");
    git_ok(
        upstream.path(),
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            git_bare.path().to_str().unwrap(),
        ],
    );

    let grit_bare = tempfile::tempdir().expect("grit bare");
    let outcome = clone_repo(
        CloneOptions {
            bare: true,
            ..CloneOptions::new(
                upstream.path().to_string_lossy(),
                grit_bare.path().to_path_buf(),
            )
        },
        &mut NoProgress,
        None,
    )
    .expect("grit bare clone");
    assert!(outcome.bare);
    assert_eq!(outcome.checkout, CloneCheckout::Skipped);
    assert_eq!(outcome.files, 0);

    assert_eq!(
        git_stdout(git_bare.path(), &["config", "core.bare"]),
        git_stdout(grit_bare.path(), &["config", "core.bare"]),
        "core.bare must match"
    );
    assert_eq!(
        config_keys(git_bare.path()),
        config_keys(grit_bare.path()),
        "local config keys must match git clone --bare"
    );
    assert_eq!(
        for_each_ref(git_bare.path()),
        for_each_ref(grit_bare.path()),
        "for-each-ref must match git clone --bare"
    );

    git_ok(grit_bare.path(), &["fsck", "--strict"]);

    let wrapper = tempfile::tempdir().expect("wrapper");
    git_ok(
        wrapper.path(),
        &[
            "clone",
            "-q",
            grit_bare.path().to_str().unwrap(),
            "from-grit",
        ],
    );
}

#[test]
fn clone_no_checkout_leaves_no_index() {
    let upstream = tempfile::tempdir().expect("upstream");
    git_ok(upstream.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.path().join("f.txt"), b"hello\n").expect("write");
    git_ok(upstream.path(), &["add", "f.txt"]);
    git_ok(
        upstream.path(),
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "-qm",
            "c1",
        ],
    );

    let git_dest = tempfile::tempdir().expect("git dest");
    git_ok(
        upstream.path(),
        &[
            "clone",
            "-q",
            "--no-checkout",
            ".",
            git_dest.path().to_str().unwrap(),
        ],
    );

    let grit_dest = tempfile::tempdir().expect("grit dest");
    let outcome = clone_repo(
        CloneOptions {
            checkout: false,
            ..CloneOptions::new(
                upstream.path().to_string_lossy(),
                grit_dest.path().to_path_buf(),
            )
        },
        &mut NoProgress,
        None,
    )
    .expect("grit no-checkout clone");
    assert_eq!(outcome.checkout, CloneCheckout::Skipped);
    assert!(!grit_dest.path().join(".git/index").exists());
    assert_eq!(
        git_stdout(git_dest.path(), &["rev-parse", "HEAD"]),
        git_stdout(grit_dest.path(), &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git_stdout(git_dest.path(), &["status", "--porcelain"]),
        git_stdout(grit_dest.path(), &["status", "--porcelain"])
    );
}

#[test]
fn clone_checkout_counts_files() {
    let upstream = tempfile::tempdir().expect("upstream");
    git_ok(upstream.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.path().join("a.txt"), b"aaa\n").expect("write");
    std::fs::write(upstream.path().join("b.txt"), b"bbbb\n").expect("write");
    git_ok(upstream.path(), &["add", "."]);
    git_ok(
        upstream.path(),
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "-qm",
            "c1",
        ],
    );

    let dest = tempfile::tempdir().expect("dest");
    let outcome = clone_repo(
        CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf()),
        &mut NoProgress,
        None,
    )
    .expect("clone");
    assert!(matches!(outcome.checkout, CloneCheckout::CheckedOut(_)));
    assert_eq!(outcome.files, 2, "expected two checked-out files");
    assert!(
        outcome.worktree_bytes > 0,
        "worktree_bytes should reflect file sizes"
    );
    assert!(dest.path().join("a.txt").is_file());
    assert!(dest.path().join("b.txt").is_file());
}

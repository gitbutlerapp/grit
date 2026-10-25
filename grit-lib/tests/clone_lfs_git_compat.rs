//! `grit clone` must run LFS smudge filters with the new repository as cwd.

use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn have_git_lfs() -> bool {
    Command::new("git-lfs")
        .arg("version")
        .status()
        .is_ok_and(|s| s.success())
}

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn clone_lfs_repo_materializes_smudged_blob() {
    if !have_git_lfs() {
        return;
    }

    let base = tempfile::tempdir().expect("tempdir");
    let remote = base.path().join("remote.git");
    let work = base.path().join("work");
    assert!(git(
        base.path(),
        &["init", "-q", "--bare", remote.to_str().expect("utf8")]
    ));
    assert!(git(
        base.path(),
        &["init", "-q", "-b", "main", work.to_str().expect("utf8")]
    ));
    assert!(git(&work, &["lfs", "install", "--local"]));
    assert!(git(&work, &["lfs", "track", "*.dat"]));
    std::fs::write(work.join("a.dat"), vec![0u8; 50_000]).expect("a.dat");
    std::fs::write(work.join("t.txt"), b"hi\n").expect("t.txt");
    assert!(git(&work, &["add", "-A"]));
    assert!(git(
        &work,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e.invalid",
            "commit",
            "-qm",
            "lfs"
        ]
    ));
    assert!(git(
        &work,
        &["remote", "add", "origin", remote.to_str().expect("utf8")]
    ));
    assert!(git(&work, &["push", "-u", "origin", "main"]));
    assert!(git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]));

    let vars = std::env::vars_os().filter(|(k, _)| k != "GIT_LFS_SKIP_SMUDGE");
    let env = Environment::from_vars(vars, base.path().to_path_buf());

    let dest = base.path().join("grit-clone");
    let opts = CloneOptions {
        url: remote.to_string_lossy().into_owned(),
        dest: dest.clone(),
        environment: env,
        ..CloneOptions::new(remote.to_string_lossy(), dest.clone())
    };
    clone_repo(opts, &mut NoProgress, &NeverCancel, None).expect("grit clone");

    let len = std::fs::metadata(dest.join("a.dat"))
        .expect("a.dat exists")
        .len();
    assert_eq!(len, 50_000, "LFS smudge should materialize full blob");
}

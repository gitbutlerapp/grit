//! Local clone transfer stats when the source mixes a linked pack with a new pack.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::Environment;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
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

fn rev_list_all_objects(git_dir: &Path) -> usize {
    let out = Command::new("git")
        .current_dir(git_dir.parent().unwrap_or(git_dir))
        .args([
            "-C",
            &git_dir.to_string_lossy(),
            "rev-list",
            "--all",
            "--objects",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-list");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

#[test]
fn clone_local_repacked_plus_loose_reports_full_transfer_stats() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@e"]);
    for i in 1..=5 {
        std::fs::write(work.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", &format!("c{i}")]);
    }

    let bare = tmp.path().join("w.git");
    git(tmp.path(), &["clone", "-q", "--bare", "w", "w.git"]);
    git(&bare, &["repack", "-a", "-d", "-q"]);

    std::fs::write(work.join("f6"), "6\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "c6"]);
    git(&work, &["push", "-q", "../w.git", "HEAD"]);

    let dest = tmp.path().join("c5");
    let outcome = clone_repo(
        CloneOptions {
            ..CloneOptions::new(bare.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone");

    let dest_git = dest.join(".git");
    let expected_objects = rev_list_all_objects(&dest_git);
    assert_eq!(
        expected_objects, 18,
        "fixture must expose 18 reachable objects"
    );
    assert_eq!(
        outcome.transfer.objects,
        u64::try_from(expected_objects).expect("count"),
        "transfer stats must count linked and newly ingested packs"
    );
    let kinds = outcome.transfer.kinds.expect("object type breakdown");
    assert_eq!(kinds.commits, 6);
    assert_eq!(kinds.total(), outcome.transfer.objects);
    assert!(outcome.transfer.bytes > 0);

    git(&dest_git, &["fsck", "--strict"]);
}

//! Coverage for `repo.status()`, `repo.index()` staging, and `repo.commits()`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::commits::{CommitOptions, CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::status::{StatusOptions, UntrackedCachePolicy, UntrackedOptions};

fn git(args: &[&str], cwd: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {:?}", args);
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn status_run_and_untracked_cache_parity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = dir.path();
    fs::write(wt.join("tracked.txt"), "a\n").expect("write");
    git(&["add", "tracked.txt"], wt);
    fs::write(wt.join("new.txt"), "b\n").expect("untracked");

    let mut progress = NullProgress;
    let cancel = CancelToken::never();
    let model = repo
        .status()
        .run(&StatusOptions::default(), &mut progress, &cancel)
        .expect("status");
    assert_eq!(model.untracked, vec!["new.txt".to_owned()]);

    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let with_cache = UntrackedOptions::default().with_cache(UntrackedCachePolicy::Refresh);
    let without_cache = UntrackedOptions::default().with_cache(UntrackedCachePolicy::Skip);
    let (u1, _) = repo
        .status()
        .untracked(&mut index, &with_cache, &mut progress, &cancel)
        .expect("untracked cache");
    let (u2, _) = repo
        .status()
        .untracked(&mut index, &without_cache, &mut progress, &cancel)
        .expect("untracked no cache");
    assert_eq!(u1, u2);
    assert!(u1.contains(&"new.txt".to_owned()));
}

#[test]
fn commit_staged_vs_autostage_roundtrip_git_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = dir.path();
    fs::write(wt.join("f.txt"), "one\n").expect("write");
    repo.index()
        .stage(
            &grit_lib::porcelain::add::StageOptions {
                pathspecs: vec!["f.txt".into()],
                ..Default::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage");

    let id = "Test User <t@example.com> 1700000000 +0000";
    let mut req = CommitRequest::new("staged only");
    req.author = Some(id.parse().expect("signature"));
    req.committer = Some(id.parse().expect("signature"));
    let mut progress = NullProgress;
    let cancel = CancelToken::never();
    let staged = repo
        .commits()
        .create(
            &CommitOptions::new(req.clone(), CommitScope::Staged),
            &mut progress,
            &cancel,
        )
        .expect("staged commit");
    git(&["fsck", "--strict"], wt);
    let msg = git(&["log", "-1", "--format=%B"], wt);
    assert!(msg.contains("staged only"));

    fs::write(wt.join("g.txt"), "two\n").expect("write");
    let auto = repo
        .commits()
        .create(
            &CommitOptions::new(
                {
                    let id = "Test User <t@example.com> 1700000001 +0000";
                    let mut r = CommitRequest::new("auto stage");
                    r.author = Some(id.parse().expect("signature"));
                    r.committer = Some(id.parse().expect("signature"));
                    r
                },
                CommitScope::AutoStage,
            ),
            &mut progress,
            &cancel,
        )
        .expect("autostage");
    assert_ne!(auto.oid, staged.oid);
    git(&["fsck", "--strict"], wt);
    let auto_msg = git(&["log", "-1", "--format=%B"], wt);
    assert!(auto_msg.contains("auto stage"));
}

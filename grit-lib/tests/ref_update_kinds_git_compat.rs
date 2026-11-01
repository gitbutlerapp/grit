//! [`UpdateMode::kind`] and [`PushOutcome`]'s summaries on real fetches and pushes
//! against repositories the system `git` builds and then checks with `fsck`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use grit_lib::remote::transfer::{
    fetch_local, push_local, FetchOptions, FetchOutcome, PushOptions, PushRefSpec, TagMode,
    UpdateKind,
};
use grit_test_support::{git, rev_parse};

fn commit(dir: &Path, file: &str, contents: &str) {
    std::fs::write(dir.join(file), contents).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", file]);
}

fn kind_of(outcome: &FetchOutcome, remote_ref: &str) -> Option<UpdateKind> {
    outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == remote_ref)
        .map(|u| u.mode.kind())
}

fn fetch_options(prune: bool) -> FetchOptions {
    let mut opts = FetchOptions::new();
    opts.refspecs = vec![
        "refs/heads/main:refs/remotes/origin/main".to_owned(),
        "+refs/heads/topic:refs/remotes/origin/topic".to_owned(),
        "+refs/heads/gone:refs/remotes/origin/gone".to_owned(),
        "refs/tags/*:refs/tags/*".to_owned(),
    ];
    opts.tags = TagMode::None;
    opts.prune = prune;
    opts
}

#[test]
fn fetch_kinds_follow_what_happened_to_each_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    commit(&remote, "a.txt", "a\n");
    git(&remote, &["branch", "topic"]);
    git(&remote, &["branch", "gone"]);
    git(&remote, &["tag", "v1"]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let first = fetch_local(&local_git, &remote_git, &fetch_options(false)).expect("fetch");
    for name in [
        "refs/heads/main",
        "refs/heads/topic",
        "refs/heads/gone",
        "refs/tags/v1",
    ] {
        assert_eq!(kind_of(&first, name), Some(UpdateKind::New), "{name}");
    }
    assert_eq!(first.changed_ref_count(), 4);
    assert!(!first.has_rejections());

    commit(&remote, "b.txt", "b\n");
    git(&remote, &["switch", "-q", "topic"]);
    git(&remote, &["commit", "-q", "--amend", "-m", "rewritten"]);
    git(&remote, &["switch", "-q", "main"]);
    git(&remote, &["branch", "-D", "gone"]);
    git(&remote, &["tag", "-f", "v1", "HEAD"]);

    let mut dry_run = fetch_options(true);
    dry_run.dry_run = true;
    let dry = fetch_local(&local_git, &remote_git, &dry_run).expect("dry-run fetch");
    assert!(dry.changed_ref_count() > 0);
    assert_eq!(
        rev_parse(&local, "refs/remotes/origin/main"),
        rev_parse(&remote, "HEAD~1"),
        "a dry run leaves the tracking ref alone"
    );

    let second = fetch_local(&local_git, &remote_git, &fetch_options(true)).expect("fetch 2");
    assert_eq!(
        kind_of(&second, "refs/heads/main"),
        Some(UpdateKind::FastForward)
    );
    assert_eq!(
        kind_of(&second, "refs/heads/topic"),
        Some(UpdateKind::Forced)
    );
    assert_eq!(
        kind_of(&second, "refs/tags/v1"),
        Some(UpdateKind::Rejected),
        "a moved tag is not replaced without force"
    );
    let pruned = second
        .updates
        .iter()
        .find(|u| u.local_ref.as_deref() == Some("refs/remotes/origin/gone"))
        .expect("pruned tracking ref reported");
    assert_eq!(pruned.mode.kind(), UpdateKind::Pruned);
    assert!(second.has_rejections());
    assert_eq!(
        second.changed_ref_count(),
        second
            .updates
            .iter()
            .filter(|u| u.mode.kind().changes_ref())
            .count()
    );
    assert_eq!(dry.changed_ref_count(), second.changed_ref_count());

    git(&local, &["fsck", "--strict"]);
}

#[test]
fn push_outcome_summaries_match_the_remote_refs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    commit(&local, "a.txt", "a\n");
    commit(&local, "b.txt", "b\n");
    let local_git = local.join(".git");
    let tip = rev_parse(&local, "HEAD");
    let spec = |src| PushRefSpec {
        src: Some(src),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    };
    let push = |src| {
        push_local(
            &local_git,
            &remote,
            &[spec(src)],
            &PushOptions::default(),
            &grit_lib::progress::NeverCancel,
        )
        .expect("push")
    };

    let created = push(tip);
    assert!(created.has_updates());
    assert!(!created.has_rejections());
    assert_eq!(git(&remote, &["rev-parse", "main"]).trim(), tip.to_hex());

    let again = push(tip);
    assert!(!again.has_updates(), "an up-to-date push changes nothing");
    assert!(!again.has_rejections());

    let behind = rev_parse(&local, "HEAD~1");
    let refused = push(behind);
    assert!(!refused.has_updates());
    assert!(refused.has_rejections(), "{:?}", refused.results);
    assert_eq!(
        git(&remote, &["rev-parse", "main"]).trim(),
        tip.to_hex(),
        "a refused push leaves the remote ref alone"
    );

    git(&remote, &["fsck", "--strict"]);
}

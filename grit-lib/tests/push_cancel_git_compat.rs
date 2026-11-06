//! Cancelled `file://` pushes against repositories built and checked by the system
//! `git`: a cancel leaves the remote as `git` last saw it, and the retry that
//! follows writes a remote `git fsck --strict` accepts.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::progress::CancelToken;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::remote::{Remote, RemoteError};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd, git_packed_history, rev_parse};
use std::path::Path;

fn cancel_after_checks(limit: usize) -> CancelToken {
    CancelToken::after_polls(limit.saturating_add(1) as u64)
}

fn push(
    local: &Repository,
    remote_dir: &Path,
    tip: ObjectId,
    cancel: &CancelToken,
) -> Result<Vec<PushRefStatus>, RemoteError> {
    let remote = Remote::from_url(&format!("file://{}", remote_dir.display())).expect("url");
    let spec = PushRefSpec {
        src: Some(tip),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    };
    remote
        .push(
            local,
            &[spec],
            PushOptions::default(),
            &mut NullProgress,
            cancel,
            None,
        )
        .map(|outcome| outcome.results.into_iter().map(|r| r.status).collect())
}

/// How many checks a full push of `tip` into a fresh remote makes.
fn checks_for_full_push(local: &Repository, scratch: &Path, tip: ObjectId) -> usize {
    git(scratch, &["init", "-q", "--bare", "probe.git"]);
    let (counting, checks) = CancelToken::with_check_counter();
    assert_eq!(
        push(local, &scratch.join("probe.git"), tip, &counting).expect("probe push"),
        vec![PushRefStatus::Ok]
    );
    checks.load(std::sync::atomic::Ordering::Relaxed)
}

/// `refs` listing and loose/packed object counts, as `git` reports them.
fn git_state(repo: &Path) -> (String, String) {
    (
        git(repo, &["for-each-ref"]),
        git(repo, &["count-objects", "-v"]),
    )
}

fn fsck_clean(repo: &Path) {
    let out = git_cmd(&["fsck", "--strict", "--no-progress"])
        .in_dir(repo)
        .exec();
    assert!(out.ok(), "git fsck: {}{}", out.stdout, out.stderr);
}

/// A packed history on `main` plus a `side` branch in the same pack, so pushing
/// `main` alone cannot hard-link the pack and must build one.
fn packed_source(dir: &Path) -> (Repository, ObjectId) {
    let src = git_packed_history(dir, 150);
    let side = "commit refs/heads/side\ncommitter T <t@example.com> 2000000000 +0000\n\
                data 5\nside\nfrom refs/heads/main\nM 100644 inline side.txt\ndata 5\nside\n\n";
    git_cmd(&["fast-import", "--quiet"])
        .in_dir(&src)
        .stdin(side)
        .suc();
    git(&src, &["repack", "-adq"]);
    let repo = Repository::open(&src, &RepositoryOptions::empty()).expect("open source");
    let tip = rev_parse(&src, "refs/heads/main");
    (repo, tip)
}

#[test]
fn cancelled_full_push_leaves_an_empty_remote_and_the_retry_fscks_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (local, tip) = packed_source(tmp.path());
    let total = checks_for_full_push(&local, tmp.path(), tip);

    git(tmp.path(), &["init", "-q", "--bare", "dst.git"]);
    let dst = tmp.path().join("dst.git");
    let before = git_state(&dst);
    for limit in [total / 4, total / 2, total * 3 / 4] {
        let err = push(&local, &dst, tip, &cancel_after_checks(limit)).expect_err("cancelled");
        assert!(err.is_cancelled(), "cancel at check {limit}: {err:?}");
        assert_eq!(git_state(&dst), before, "cancel at check {limit}");
    }

    assert_eq!(
        push(&local, &dst, tip, &CancelToken::never()).expect("retry"),
        vec![PushRefStatus::Ok]
    );
    fsck_clean(&dst);
    assert_eq!(rev_parse(&dst, "refs/heads/main"), tip);
    assert_eq!(git(&dst, &["rev-list", "--count", "main"]).trim(), "150");
    let side = rev_parse(&tmp.path().join("history.git"), "refs/heads/side");
    let out = git_cmd(&["cat-file", "-e", &side.to_hex()])
        .in_dir(&dst)
        .exec();
    assert!(
        !out.ok(),
        "objects only `side` reaches stay out of the remote"
    );
}

#[test]
fn cancelled_incremental_push_onto_a_git_pushed_remote_keeps_its_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (local, tip) = packed_source(tmp.path());
    let src = tmp.path().join("history.git");
    git(tmp.path(), &["init", "-q", "--bare", "dst.git"]);
    let dst = tmp.path().join("dst.git");
    git(
        &src,
        &[
            "push",
            "-q",
            &dst.to_string_lossy(),
            "main~60:refs/heads/main",
        ],
    );
    let old = rev_parse(&dst, "refs/heads/main");

    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).expect("scratch");
    git(&scratch, &["init", "-q", "--bare", "probe.git"]);
    let probe = scratch.join("probe.git");
    git(
        &src,
        &[
            "push",
            "-q",
            &probe.to_string_lossy(),
            "main~60:refs/heads/main",
        ],
    );
    let (counting, checks) = CancelToken::with_check_counter();
    assert_eq!(
        push(&local, &probe, tip, &counting).expect("probe push"),
        vec![PushRefStatus::Ok]
    );
    let total = checks.load(std::sync::atomic::Ordering::Relaxed);

    let before = git_state(&dst);
    for limit in [total / 4, total / 2, total * 3 / 4] {
        let err = push(&local, &dst, tip, &cancel_after_checks(limit)).expect_err("cancelled");
        assert!(err.is_cancelled(), "cancel at check {limit}: {err:?}");
        assert_eq!(git_state(&dst), before, "cancel at check {limit}");
    }
    assert_eq!(rev_parse(&dst, "refs/heads/main"), old);

    assert_eq!(
        push(&local, &dst, tip, &CancelToken::never()).expect("retry"),
        vec![PushRefStatus::Ok]
    );
    fsck_clean(&dst);
    assert_eq!(rev_parse(&dst, "refs/heads/main"), tip);
}

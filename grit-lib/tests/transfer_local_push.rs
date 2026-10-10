//! Integration tests for `grit_lib::transfer::push_local` — the in-process
//! local / `file://` push (send-pack) path (no subprocess, no wire protocol).
//!
//! A bare remote repo and a normal local repo with commits are built with the
//! system `git`. `push_local` copies the minimal object set into the remote odb
//! and moves the remote ref. We assert the remote ref + objects, that
//! `git fsck` on the remote stays clean, and the rejection semantics for
//! non-fast-forward, force, delete, and force-with-lease (CAS) pushes — cross
//! checking remote state with the system `git`.

use std::path::Path;

use grit_lib::objects::ObjectId;
use grit_lib::push_report::PushRefStatus;
use grit_lib::repo::Repository;
use grit_lib::transfer::{push_local, PushOptions, PushRefSpec};
use grit_lib::transport_path::resolve_local_remote_git_dir;
use grit_test_support::{git, git_cmd};

/// `git` that may fail; returns whether it succeeded plus combined output.
fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_cmd(args).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

/// The remote's current value of `dst`, via the system git, or `None` if absent.
fn remote_ref(remote_git: &Path, dst: &str) -> Option<ObjectId> {
    let (ok, out) = git_try(remote_git, &["rev-parse", "--verify", "-q", dst]);
    if ok {
        ObjectId::from_hex(out.trim()).ok()
    } else {
        None
    }
}

fn fsck_clean(remote_git: &Path) {
    let (ok, out) = git_try(remote_git, &["fsck", "--strict"]);
    assert!(ok, "git fsck on remote not clean: {out}");
}

#[test]
fn push_local_full_lifecycle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    // Bare remote (its own git dir).
    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    // Local repo with two commits on main.
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    std::fs::write(local.join("b.txt"), "two\n").unwrap();
    git(&local, &["add", "b.txt"]);
    git(&local, &["commit", "-q", "-m", "c2"]);

    let c2 = rev_parse(&local, "refs/heads/main");
    let c1 = rev_parse(&local, "HEAD~1");
    let b_blob = rev_parse(&local, "HEAD:b.txt");

    // --- (a) push refs/heads/main: a new ref. ---
    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("push main");

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(r.status, PushRefStatus::Ok);
    assert!(!r.forced);
    assert_eq!(r.new_oid, Some(c2));
    assert!(r.old_oid.is_none(), "new ref has no old oid");

    // Remote ref + objects exist; fsck clean.
    assert_eq!(remote_ref(remote_git, "refs/heads/main"), Some(c2));
    assert!(git_try(remote_git, &["cat-file", "-e", &c2.to_hex()]).0);
    assert!(git_try(remote_git, &["cat-file", "-e", &c1.to_hex()]).0);
    assert!(
        git_try(remote_git, &["cat-file", "-e", &b_blob.to_hex()]).0,
        "b.txt blob copied to remote"
    );
    fsck_clean(remote_git);

    // --- (b) non-fast-forward rewrite, pushed without force: rejected. ---
    // Rewrite local main: amend onto c1 so the new tip is not a descendant of c2.
    git(&local, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(local.join("d.txt"), "four\n").unwrap();
    git(&local, &["add", "d.txt"]);
    git(&local, &["commit", "-q", "-m", "c2-prime"]);
    let c2_prime = rev_parse(&local, "refs/heads/main");
    assert_ne!(c2_prime, c2);
    // Sanity: c2 is not an ancestor of c2_prime (true non-ff).
    let (anc, _) = git_try(
        &local,
        &[
            "merge-base",
            "--is-ancestor",
            &c2.to_hex(),
            &c2_prime.to_hex(),
        ],
    );
    assert!(!anc, "rewrite must be non-fast-forward");

    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2_prime),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("non-ff push call");
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::RejectNonFastForward
    );
    // Remote ref unchanged.
    assert_eq!(remote_ref(remote_git, "refs/heads/main"), Some(c2));

    // --- (c) same rewrite with force: accepted (forced). ---
    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2_prime),
            dst: "refs/heads/main".to_owned(),
            force: true,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("forced push call");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert!(outcome.results[0].forced, "forced flag set");
    assert_eq!(remote_ref(remote_git, "refs/heads/main"), Some(c2_prime));
    assert!(git_try(remote_git, &["cat-file", "-e", &c2_prime.to_hex()]).0);
    fsck_clean(remote_git);

    // --- (d) delete the ref. ---
    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: true,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("delete push call");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert!(outcome.results[0].deletion);
    assert_eq!(
        remote_ref(remote_git, "refs/heads/main"),
        None,
        "ref deleted"
    );

    // --- (e) CAS push with a wrong expected_old: rejected as stale, no change. ---
    // Re-establish a ref first so there's something to compare-and-swap.
    git(remote_git, &["update-ref", "refs/heads/cas", &c1.to_hex()]);
    assert_eq!(remote_ref(remote_git, "refs/heads/cas"), Some(c1));

    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2_prime),
            dst: "refs/heads/cas".to_owned(),
            force: true,
            delete: false,
            // Expect c2 but remote is actually c1 -> stale.
            expected_old: Some(c2),
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("cas push call");
    assert_eq!(outcome.results[0].status, PushRefStatus::RejectStale);
    // No change despite force=true, because the CAS check fails first.
    assert_eq!(remote_ref(remote_git, "refs/heads/cas"), Some(c1));

    // A CAS push with the correct expected_old succeeds.
    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2_prime),
            dst: "refs/heads/cas".to_owned(),
            force: true,
            delete: false,
            expected_old: Some(c1),
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("cas push call (matching)");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(remote_ref(remote_git, "refs/heads/cas"), Some(c2_prime));

    fsck_clean(remote_git);
}

#[test]
fn push_local_atomic_rejects_all_on_any_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&local, "refs/heads/main");
    std::fs::write(local.join("b.txt"), "two\n").unwrap();
    git(&local, &["add", "b.txt"]);
    git(&local, &["commit", "-q", "-m", "c2"]);
    let c2 = rev_parse(&local, "refs/heads/main");

    // Seed the remote so a CAS mismatch can be staged on one ref. Push c1 first
    // (which copies its objects), then point `locked` at it.
    push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c1),
            dst: "refs/heads/locked".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("seed locked ref");
    assert_eq!(remote_ref(remote_git, "refs/heads/locked"), Some(c1));

    // One acceptable new ref + one stale CAS ref, pushed atomically.
    let outcome = push_local(
        &local_git,
        remote_git,
        &[
            PushRefSpec {
                src: Some(c2),
                dst: "refs/heads/fresh".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            },
            PushRefSpec {
                src: Some(c2),
                dst: "refs/heads/locked".to_owned(),
                force: false,
                delete: false,
                expected_old: Some(c2), // wrong: remote is c1
                expect_absent: false,
            },
        ],
        &PushOptions {
            atomic: true,
            dry_run: false,
            ..PushOptions::default()
        },
    )
    .expect("atomic push call");

    let fresh = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/fresh")
        .unwrap();
    let locked = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/locked")
        .unwrap();
    assert_eq!(locked.status, PushRefStatus::RejectStale);
    assert_eq!(
        fresh.status,
        PushRefStatus::AtomicPushFailed,
        "accepted ref demoted under atomic failure"
    );

    // Nothing applied: the fresh ref must not exist, locked unchanged.
    assert_eq!(remote_ref(remote_git, "refs/heads/fresh"), None);
    assert_eq!(remote_ref(remote_git, "refs/heads/locked"), Some(c1));
    fsck_clean(remote_git);
}

#[test]
fn push_local_non_atomic_df_applies_first_ref_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("f.txt"), "x\n").unwrap();
    git(&local, &["add", "f.txt"]);
    git(&local, &["commit", "-q", "-m", "tip"]);
    let tip = rev_parse(&local, "refs/heads/main");

    let outcome = push_local(
        &local_git,
        remote_git,
        &[
            PushRefSpec {
                src: Some(tip),
                dst: "refs/heads/parent".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            },
            PushRefSpec {
                src: Some(tip),
                dst: "refs/heads/parent/child".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            },
        ],
        &PushOptions::default(),
    )
    .expect("non-atomic df push");

    let parent = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/parent")
        .expect("parent result");
    let child = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/parent/child")
        .expect("child result");
    assert_eq!(parent.status, PushRefStatus::Ok);
    assert_eq!(child.status, PushRefStatus::RemoteRejected);
    assert_eq!(remote_ref(remote_git, "refs/heads/parent"), Some(tip));
    assert_eq!(remote_ref(remote_git, "refs/heads/parent/child"), None);
    fsck_clean(remote_git);
}

#[test]
fn push_local_atomic_store_batch_df_marks_atomic_push_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("f.txt"), "x\n").unwrap();
    git(&local, &["add", "f.txt"]);
    git(&local, &["commit", "-q", "-m", "tip"]);
    let tip = rev_parse(&local, "refs/heads/main");

    let outcome = push_local(
        &local_git,
        remote_git,
        &[
            PushRefSpec {
                src: Some(tip),
                dst: "refs/heads/parent".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            },
            PushRefSpec {
                src: Some(tip),
                dst: "refs/heads/parent/child".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
            },
        ],
        &PushOptions {
            atomic: true,
            ..PushOptions::default()
        },
    )
    .expect("atomic df push");

    let parent = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/parent")
        .expect("parent result");
    let child = outcome
        .results
        .iter()
        .find(|r| r.remote_ref == "refs/heads/parent/child")
        .expect("child result");
    assert_eq!(
        child.status,
        PushRefStatus::RemoteRejected,
        "D/F batch should reject the nested ref"
    );
    assert_eq!(
        parent.status,
        PushRefStatus::AtomicPushFailed,
        "otherwise-accepted ref demoted when atomic batch fails"
    );
    assert_eq!(remote_ref(remote_git, "refs/heads/parent"), None);
    assert_eq!(remote_ref(remote_git, "refs/heads/parent/child"), None);
}

#[test]
fn push_local_updates_remote_tracking_ref_when_configured() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&local, "refs/heads/main");

    push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c1),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions {
            tracking_remote: Some("origin".to_owned()),
            ..PushOptions::default()
        },
    )
    .expect("push with tracking");

    assert_eq!(
        remote_ref(&local_git, "refs/remotes/origin/main"),
        Some(c1),
        "local remote-tracking ref should match the pushed tip"
    );
}

#[test]
fn push_local_up_to_date_refreshes_stale_remote_tracking_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    git(
        &local,
        &["remote", "add", "origin", remote_git.to_str().unwrap()],
    );
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    std::fs::write(local.join("b.txt"), "two\n").unwrap();
    git(&local, &["add", "b.txt"]);
    git(&local, &["commit", "-q", "-m", "c2"]);

    let c1 = rev_parse(&local, "HEAD~1");
    let c2 = rev_parse(&local, "refs/heads/main");

    push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions {
            tracking_remote: Some("origin".to_owned()),
            ..PushOptions::default()
        },
    )
    .expect("initial push");

    // Stale tracking ref: remote is at c2 but we still record c1 locally.
    grit_lib::refs::write_ref(&local_git, "refs/remotes/origin/main", &c1).expect("stale tracking");

    let outcome = push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c2),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions {
            tracking_remote: Some("origin".to_owned()),
            ..PushOptions::default()
        },
    )
    .expect("up-to-date push");

    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);
    assert_eq!(
        remote_ref(&local_git, "refs/remotes/origin/main"),
        Some(c2),
        "up-to-date push should refresh stale remote-tracking ref"
    );
}

#[test]
fn push_local_honors_custom_fetch_refspec_for_tracking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    git(
        &local,
        &["remote", "add", "origin", remote_git.to_str().unwrap()],
    );
    git(&local, &["config", "--unset-all", "remote.origin.fetch"]);
    git(
        &local,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/*:refs/custom/origin/*",
        ],
    );
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&local, "refs/heads/main");

    push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(c1),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions {
            tracking_remote: Some("origin".to_owned()),
            ..PushOptions::default()
        },
    )
    .expect("push with custom tracking mapping");

    assert_eq!(
        remote_ref(&local_git, "refs/custom/origin/main"),
        Some(c1),
        "tracking ref should follow fetch refspec destination"
    );
    assert_eq!(
        remote_ref(&local_git, "refs/remotes/origin/main"),
        None,
        "must not write the conventional refs/remotes path when fetch maps elsewhere"
    );
}

#[test]
fn push_local_missing_remote_is_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&local, "HEAD");

    let missing = tmp.path().join("no-such-remote.git");
    let err = push_local(
        &local_git,
        &missing,
        &[PushRefSpec {
            src: Some(c1),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect_err("push to missing remote must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("could not find repository"),
        "unexpected error: {msg}"
    );
    assert!(!missing.exists(), "must not create a new repository path");
}

#[test]
fn push_local_does_not_use_decoy_repo_when_parent_relative_target_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path();
    let clone_wt = parent.join("clone");
    let local_git = clone_wt.join(".git");
    std::fs::create_dir_all(&clone_wt).unwrap();

    git(&clone_wt, &["init", "-q", "-b", "main", "."]);
    std::fs::write(clone_wt.join("a.txt"), "one\n").unwrap();
    git(&clone_wt, &["add", "a.txt"]);
    git(&clone_wt, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&clone_wt, "HEAD");

    // Valid bare repo inside the worktree — must not be used when remote is `../wrong.git`.
    let decoy = clone_wt.join("wrong.git");
    std::fs::create_dir_all(&decoy).unwrap();
    git(&decoy, &["init", "-q", "--bare", "-b", "main", "."]);

    let intended = parent.join("wrong.git");
    assert!(!intended.exists());

    let remote_git = grit_lib::transport_path::resolve_local_remote_git_dir(
        "../wrong.git",
        &local_git,
        Some(&clone_wt),
    );
    assert_eq!(remote_git, clone_wt.join("../wrong.git"));

    let err = push_local(
        &local_git,
        &remote_git,
        &[PushRefSpec {
            src: Some(c1),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect_err("missing ../wrong.git must fail");
    assert!(
        err.to_string().contains("could not find repository"),
        "{}",
        err
    );
    assert_eq!(remote_ref(&decoy, "refs/heads/main"), None);
}

#[test]
fn linked_worktree_resolves_relative_remote_from_worktree_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main", "."]);

    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main", "."]);
    std::fs::write(seed.join("f"), "1\n").unwrap();
    git(&seed, &["add", "f"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main", "."]);
    git(&main, &["remote", "add", "origin", "../origin.git"]);
    git(&main, &["fetch", "origin"]);
    git(&main, &["reset", "--hard", "origin/main"]);

    let linked = root.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().unwrap(),
            "main",
        ],
    );

    let repo = Repository::discover(Some(&linked)).expect("discover linked worktree");
    assert!(
        repo.work_tree
            .as_ref()
            .is_some_and(|wt| wt.ends_with("linked")),
        "expected work_tree at linked checkout, got {:?}",
        repo.work_tree
    );

    let resolved =
        resolve_local_remote_git_dir("../origin.git", &repo.git_dir, repo.work_tree.as_deref());
    assert_eq!(
        resolved.canonicalize().expect("origin path"),
        origin.canonicalize().expect("origin canonical")
    );

    let head = rev_parse(&linked, "HEAD");
    let outcome = push_local(
        &repo.git_dir,
        &resolved,
        &[PushRefSpec {
            src: Some(head),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("push from linked worktree with resolved remote");
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);
}

fn git_count_objects(git_dir: &Path) -> (u32, u32) {
    let out = git(git_dir, &["count-objects", "-v"]);
    let mut loose = 0u32;
    let mut packs = 0u32;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("count: ") {
            loose = v.trim().parse().expect("count");
        }
        if let Some(v) = line.strip_prefix("packs: ") {
            packs = v.trim().parse().expect("packs");
        }
    }
    (loose, packs)
}

#[test]
fn push_local_installs_single_pack_without_loose_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let remote_git = remote.as_path();

    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    for i in 0..6 {
        let mut body = String::from("shared header for delta tests\n");
        body.push_str(&"z".repeat(2048));
        body.push_str(&format!("file-{i}\n"));
        std::fs::write(local.join(format!("blob{i}.txt")), body).unwrap();
        git(&local, &["add", "."]);
        git(&local, &["commit", "-q", "-m", &format!("commit {i}")]);
    }
    git(
        &local,
        &[
            "repack",
            "-q",
            "-a",
            "-d",
            "-f",
            "--window=10",
            "--depth=50",
        ],
    );
    let tip = rev_parse(&local, "refs/heads/main");

    push_local(
        &local_git,
        remote_git,
        &[PushRefSpec {
            src: Some(tip),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
    )
    .expect("push to empty bare remote");

    let (loose, packs) = git_count_objects(remote_git);
    assert_eq!(
        loose, 0,
        "remote must not store pushed objects as loose files"
    );
    assert_eq!(packs, 1, "remote must retain one packfile");
    fsck_clean(remote_git);
}

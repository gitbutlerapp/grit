//! Local-path push must honor the remote's `receive.*` policy, matching wire
//! receive-pack and system `git push` to the same repository.

use grit_test_support::rev_parse;
use std::io::Write;
use std::path::Path;

use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_test_support::{git, git_cmd};

fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_cmd(args).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

fn git_config(repo: &Path, key: &str, value: &str) {
    git(repo, &["config", key, value]);
}

#[test]
fn push_local_honors_deny_non_fast_forwards() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    let bare = tmp.path().join("r.git");
    let local = tmp.path().join("w");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(seed.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&seed, &["branch", "feature"]);
    git_cmd(&["clone", "-q", "--bare", "seed", "r.git"])
        .in_dir(tmp.path())
        .suc();
    git_config(&bare, "receive.denyNonFastForwards", "true");

    git_cmd(&["clone", "-q", "r.git", "w"])
        .in_dir(tmp.path())
        .suc();
    let local_git = local.join(".git");
    git(&local, &["reset", "-q", "--hard", "HEAD~2"]);
    std::fs::write(local.join("q"), "q\n").unwrap();
    git(&local, &["add", "q"]);
    git(&local, &["commit", "-q", "-m", "div"]);
    let div = rev_parse(&local, "HEAD");
    let remote_c3 = rev_parse(&bare, "refs/heads/main");

    let outcome = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &[PushRefSpec {
            src: Some(div),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: true,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(
        outcome.results[0]
            .message
            .as_deref()
            .is_some_and(|m| m.contains("non-fast-forward")),
        "expected non-fast-forward refusal, got {:?}",
        outcome.results[0].message
    );
    assert_eq!(rev_parse(&bare, "refs/heads/main"), remote_c3);

    let (ok, out) = git_try(&local, &["push", "--force", "origin", "main"]);
    assert!(!ok, "git push --force should fail: {out}");
}

#[test]
fn push_local_honors_deny_deletes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    let bare = tmp.path().join("r.git");
    let local = tmp.path().join("w");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main", "."]);
    std::fs::write(seed.join("a"), "a\n").unwrap();
    git(&seed, &["add", "a"]);
    git(&seed, &["commit", "-q", "-m", "c1"]);
    git(&seed, &["branch", "feature"]);
    git_cmd(&["clone", "-q", "--bare", "seed", "r.git"])
        .in_dir(tmp.path())
        .suc();
    git_config(&bare, "receive.denyDeletes", "true");

    git_cmd(&["clone", "-q", "r.git", "w"])
        .in_dir(tmp.path())
        .suc();
    let local_git = local.join(".git");
    let feature_tip = rev_parse(&bare, "refs/heads/feature");

    let outcome = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &[PushRefSpec {
            src: None,
            src_ref: None,
            dst: "refs/heads/feature".to_owned(),
            force: false,
            delete: true,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("deletion prohibited")));
    assert_eq!(rev_parse(&bare, "refs/heads/feature"), feature_tip);
}

#[test]
fn push_local_honors_deny_current_branch_on_non_bare() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("nb");
    let local = tmp.path().join("w2");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("a"), "1\n").unwrap();
    git(&remote, &["add", "a"]);
    git(&remote, &["commit", "-q", "-m", "1"]);

    git_cmd(&["clone", "-q", "nb", "w2"])
        .in_dir(tmp.path())
        .suc();
    let local_git = local.join(".git");
    std::fs::write(local.join("b"), "two\n").unwrap();
    git(&local, &["add", "b"]);
    git(&local, &["commit", "-q", "-m", "two"]);
    let two = rev_parse(&local, "HEAD");
    let remote_one = rev_parse(&remote, "refs/heads/main");

    let outcome = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(remote.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(two),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("checked out")));
    assert_eq!(rev_parse(&remote, "refs/heads/main"), remote_one);
}

fn git_status_short(repo: &Path) -> String {
    git(repo, &["status", "--short"]).trim().to_owned()
}

#[test]
fn push_local_update_instead_deploys_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git_config(&srv, "receive.denyCurrentBranch", "updateInstead");

    git_cmd(&["clone", "-q", "srv", "work"])
        .in_dir(tmp.path())
        .suc();
    let work_git = work.join(".git");
    std::fs::write(work.join("b"), "b\n").unwrap();
    git(&work, &["add", "b"]);
    git(&work, &["commit", "-q", "-m", "b"]);
    std::fs::create_dir_all(work.join("d")).unwrap();
    std::fs::write(work.join("d/q"), "q\n").unwrap();
    git(&work, &["add", "d"]);
    git(&work, &["commit", "-q", "-m", "d"]);
    let tip = rev_parse(&work, "HEAD");
    let srv_before = rev_parse(&srv, "refs/heads/main");

    let outcome = grit_lib::repo::Repository::open(
        &work_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(srv.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(tip),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(rev_parse(&srv, "refs/heads/main"), tip);
    assert_ne!(tip, srv_before);
    assert!(srv.join("b").is_file());
    assert!(srv.join("d/q").is_file());
    assert_eq!(git_status_short(&srv), "");
    git(&srv, &["fsck", "--strict", "--no-progress"]);
}

#[test]
fn push_local_update_instead_rejects_dirty_unstaged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git_config(&srv, "receive.denyCurrentBranch", "updateInstead");

    git_cmd(&["clone", "-q", "srv", "work"])
        .in_dir(tmp.path())
        .suc();
    let work_git = work.join(".git");
    std::fs::write(work.join("c"), "c\n").unwrap();
    git(&work, &["add", "c"]);
    git(&work, &["commit", "-q", "-m", "c"]);
    let tip = rev_parse(&work, "HEAD");
    let srv_before = rev_parse(&srv, "refs/heads/main");
    std::fs::OpenOptions::new()
        .append(true)
        .open(srv.join("a"))
        .unwrap()
        .write_all(b"dirty\n")
        .unwrap();

    let outcome = grit_lib::repo::Repository::open(
        &work_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(srv.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(tip),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("unstaged changes")));
    assert_eq!(rev_parse(&srv, "refs/heads/main"), srv_before);
    assert!(std::fs::read_to_string(srv.join("a"))
        .unwrap()
        .ends_with("dirty\n"));
}

#[test]
fn push_local_update_instead_rejects_dirty_staged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git_config(&srv, "receive.denyCurrentBranch", "updateInstead");

    git_cmd(&["clone", "-q", "srv", "work"])
        .in_dir(tmp.path())
        .suc();
    let work_git = work.join(".git");
    std::fs::write(work.join("c"), "c\n").unwrap();
    git(&work, &["add", "c"]);
    git(&work, &["commit", "-q", "-m", "c"]);
    let tip = rev_parse(&work, "HEAD");
    let srv_before = rev_parse(&srv, "refs/heads/main");
    std::fs::write(srv.join("a"), "staged\n").unwrap();
    git(&srv, &["add", "a"]);

    let outcome = grit_lib::repo::Repository::open(
        &work_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(srv.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(tip),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("staged changes")));
    assert_eq!(rev_parse(&srv, "refs/heads/main"), srv_before);
}

#[test]
fn push_local_update_instead_refuses_untracked_collision() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git_config(&srv, "receive.denyCurrentBranch", "updateInstead");

    git_cmd(&["clone", "-q", "srv", "work"])
        .in_dir(tmp.path())
        .suc();
    let work_git = work.join(".git");
    std::fs::write(work.join("collision"), "incoming\n").unwrap();
    git(&work, &["add", "collision"]);
    git(&work, &["commit", "-q", "-m", "add collision"]);
    let tip = rev_parse(&work, "HEAD");
    let srv_before = rev_parse(&srv, "refs/heads/main");
    std::fs::write(srv.join("collision"), "keep me\n").unwrap();

    let outcome = grit_lib::repo::Repository::open(
        &work_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(srv.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(tip),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("Could not update working tree to new HEAD")));
    assert_eq!(rev_parse(&srv, "refs/heads/main"), srv_before);
    assert_eq!(
        std::fs::read_to_string(srv.join("collision")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn push_local_update_instead_ignore_advances_ref_without_checkout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv = tmp.path().join("srv");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&srv).unwrap();
    git(&srv, &["init", "-q", "-b", "main", "."]);
    std::fs::write(srv.join("a"), "a\n").unwrap();
    git(&srv, &["add", "a"]);
    git(&srv, &["commit", "-q", "-m", "a"]);
    git_config(&srv, "receive.denyCurrentBranch", "ignore");

    git_cmd(&["clone", "-q", "srv", "work"])
        .in_dir(tmp.path())
        .suc();
    let work_git = work.join(".git");
    std::fs::write(work.join("b"), "b\n").unwrap();
    git(&work, &["add", "b"]);
    git(&work, &["commit", "-q", "-m", "b"]);
    let tip = rev_parse(&work, "HEAD");

    let outcome = grit_lib::repo::Repository::open(
        &work_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(srv.join(".git").as_path()),
        &[PushRefSpec {
            src: Some(tip),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            hook_src_ref: None,
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(rev_parse(&srv, "refs/heads/main"), tip);
    assert!(!srv.join("b").exists());
    assert_ne!(git_status_short(&srv), "");
}

//! Local-path push must honor the remote's `receive.*` policy, matching wire
//! receive-pack and system `git push` to the same repository.

use std::path::Path;

use grit_lib::objects::ObjectId;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec};
use grit_test_support::{git, git_cmd};

fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_cmd(args).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
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

    let outcome = push_local(
        &local_git,
        &bare,
        &[PushRefSpec {
            src: Some(div),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: true,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
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

    let outcome = push_local(
        &local_git,
        &bare,
        &[PushRefSpec {
            src: None,
            src_ref: None,
            dst: "refs/heads/feature".to_owned(),
            force: false,
            delete: true,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
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

    let outcome = push_local(
        &local_git,
        remote.join(".git").as_path(),
        &[PushRefSpec {
            src: Some(two),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push_local");

    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert!(outcome.results[0]
        .message
        .as_deref()
        .is_some_and(|m| m.contains("checked out")));
    assert_eq!(rev_parse(&remote, "refs/heads/main"), remote_one);
}

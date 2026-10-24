//! Push `--delete` of a ref absent on the remote is rejected (issue #1616).

use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{push_local, resolve_push_delete_ref, PushOptions, PushRefSpec};
use grit_test_support::{git, git_cmd};
use std::collections::HashSet;
use std::path::Path;

fn git_try(dir: &Path, args: &[&str]) -> bool {
    git_cmd(args).in_dir(dir).exec().ok()
}

#[test]
fn delete_missing_remote_branch_errors_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    git(&local, &["config", "user.name", "a"]);
    git(&local, &["config", "user.email", "a@x"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-qm", "c"]);
    git(
        &local,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&local, &["push", "-u", "origin", "main"]);

    let local_git = local.join(".git");
    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            dst: "refs/heads/no-such-branch".to_owned(),
            delete: true,
            ..Default::default()
        }],
        &PushOptions::default(),
    )
    .expect("push_local");
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::RejectRemoteRefMissing
    );

    assert!(
        !git_try(&local, &["push", "origin", "--delete", "no-such-branch"]),
        "git must reject missing delete target"
    );
}

#[test]
fn delete_resolves_local_tag_when_remote_tag_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    git(&local, &["config", "user.name", "a"]);
    git(&local, &["config", "user.email", "a@x"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-qm", "c"]);
    git(
        &local,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&local, &["push", "-u", "origin", "main"]);
    git(&local, &["tag", "-a", "v3.0.0", "-m", "rel"]);
    git(&local, &["push", "origin", "refs/tags/v3.0.0"]);
    git(&local, &["push", "origin", "--delete", "v3.0.0"]);

    let local_git = local.join(".git");
    let remote_names = HashSet::new();
    assert_eq!(
        resolve_push_delete_ref(&local_git, &remote_names, "v3.0.0"),
        "refs/tags/v3.0.0"
    );

    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            dst: "refs/tags/v3.0.0".to_owned(),
            delete: true,
            ..Default::default()
        }],
        &PushOptions::default(),
    )
    .expect("push_local");
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::RejectRemoteRefMissing
    );
    assert_eq!(outcome.results[0].remote_ref, "refs/tags/v3.0.0");
}

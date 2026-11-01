//! Push must not move an existing remote tag without `--force` (Git-compatible).

use grit_lib::objects::ObjectId;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec, PUSH_TAG_EXISTS_MESSAGE};
use grit_test_support::git;

fn tag_push_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let remote = root.join("remote.git");
    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["clone", "-q", remote.to_str().unwrap(), "."]);
    std::fs::write(local.join("f"), b"1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    std::fs::write(local.join("f"), b"1\n2\n").unwrap();
    git(&local, &["commit", "-q", "-am", "c2"]);
    let c1 = ObjectId::from_hex(git(&local, &["rev-parse", "HEAD~1"]).trim()).unwrap();
    let c2 = ObjectId::from_hex(git(&local, &["rev-parse", "HEAD"]).trim()).unwrap();
    git(&local, &["tag", "v1", &c1.to_hex()]);
    git(&local, &["push", "-q", "origin", "main", "v1"]);
    git(&local, &["tag", "-f", "v1", &c2.to_hex()]);
    (tmp, remote, local.join(".git"))
}

#[test]
fn push_local_rejects_moving_existing_lightweight_tag_without_force() {
    let (_tmp, remote, local_git) = tag_push_fixture();
    let local_wt = local_git.parent().unwrap();
    let moved = ObjectId::from_hex(git(local_wt, &["rev-parse", "refs/tags/v1"]).trim()).unwrap();
    let before = git(&remote, &["rev-parse", "--short", "refs/tags/v1"]);
    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            src: Some(moved),
            dst: "refs/tags/v1".to_owned(),
            src_ref: Some("refs/tags/v1".to_owned()),
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(r.status, PushRefStatus::RejectAlreadyExists);
    assert_eq!(r.message.as_deref(), Some(PUSH_TAG_EXISTS_MESSAGE));
    assert_eq!(
        git(&remote, &["rev-parse", "--short", "refs/tags/v1"]),
        before
    );
    assert_ne!(
        git(&remote, &["rev-parse", "refs/tags/v1"]).trim(),
        moved.to_hex()
    );
}

#[test]
fn push_local_force_moves_existing_tag_and_marks_forced() {
    let (_tmp, remote, local_git) = tag_push_fixture();
    let c2 = ObjectId::from_hex(
        git(&local_git.parent().unwrap(), &["rev-parse", "refs/tags/v1"]).trim(),
    )
    .unwrap();
    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            src: Some(c2),
            dst: "refs/tags/v1".to_owned(),
            force: true,
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert!(outcome.results[0].forced);
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/v1"]).trim(),
        c2.to_hex()
    );
}

#[test]
fn push_local_identical_tag_is_up_to_date() {
    let (_tmp, remote, local_git) = tag_push_fixture();
    git(
        local_git.parent().unwrap(),
        &[
            "tag",
            "-f",
            "v1",
            &git(&remote, &["rev-parse", "refs/tags/v1"]).trim(),
        ],
    );
    let oid = ObjectId::from_hex(git(&remote, &["rev-parse", "refs/tags/v1"]).trim()).unwrap();
    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            src: Some(oid),
            dst: "refs/tags/v1".to_owned(),
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);
}

#[test]
fn push_local_rejects_annotated_to_lightweight_on_same_commit_without_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let remote = root.join("remote.git");
    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["clone", "-q", remote.to_str().unwrap(), "."]);
    std::fs::write(local.join("f"), b"x\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    std::fs::write(local.join("f"), b"x\ny\n").unwrap();
    git(&local, &["commit", "-q", "-am", "c2"]);
    let c1 = git(&local, &["rev-parse", "HEAD~1"]).trim().to_string();
    git(&local, &["tag", "-a", "-m", "rel", "v1", &c1]);
    git(&local, &["push", "-q", "-f", "origin", "v1"]);
    let annotated = git(&remote, &["rev-parse", "refs/tags/v1"])
        .trim()
        .to_string();
    git(&local, &["tag", "-f", "v1", &c1]);
    let lightweight =
        ObjectId::from_hex(git(&local, &["rev-parse", "refs/tags/v1"]).trim()).unwrap();
    assert_eq!(lightweight.to_hex(), c1);
    let outcome = push_local(
        &local.join(".git"),
        &remote,
        &[PushRefSpec {
            src: Some(lightweight),
            dst: "refs/tags/v1".to_owned(),
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::RejectAlreadyExists
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/v1"]).trim(),
        annotated,
        "remote annotated tag object must be unchanged"
    );
    let fsck = std::process::Command::new("git")
        .args([
            "-C",
            remote.to_str().unwrap(),
            "fsck",
            "--connectivity-only",
        ])
        .output()
        .expect("fsck");
    assert!(fsck.status.success());
    let stderr = String::from_utf8_lossy(&fsck.stderr);
    assert!(
        !stderr.contains("dangling tag"),
        "unexpected dangling tag: {stderr}"
    );
}

#[test]
fn push_local_rejects_diverged_tag_as_already_exists_not_non_ff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let remote = root.join("remote.git");
    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["clone", "-q", remote.to_str().unwrap(), "."]);
    std::fs::write(local.join("a"), b"a\n").unwrap();
    git(&local, &["add", "a"]);
    git(&local, &["commit", "-q", "-m", "base"]);
    let base = git(&local, &["rev-parse", "HEAD"]).trim().to_string();
    git(&local, &["tag", "v1", &base]);
    git(&local, &["push", "-q", "origin", "v1"]);
    git(&local, &["checkout", "-q", "-b", "side"]);
    std::fs::write(local.join("b"), b"b\n").unwrap();
    git(&local, &["add", "b"]);
    git(&local, &["commit", "-q", "-m", "side"]);
    let side_tip = git(&local, &["rev-parse", "HEAD"]).trim().to_string();
    git(&local, &["tag", "-f", "v1", &side_tip]);
    let outcome = push_local(
        &local.join(".git"),
        &remote,
        &[PushRefSpec {
            src: Some(ObjectId::from_hex(&side_tip).unwrap()),
            dst: "refs/tags/v1".to_owned(),
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::RejectAlreadyExists
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/tags/v1"]).trim(),
        base,
        "remote tag must stay on base"
    );
}

#[test]
fn push_local_branch_fast_forward_unchanged() {
    let (_tmp, remote, local_git) = tag_push_fixture();
    let tip = ObjectId::from_hex(
        git(
            local_git.parent().unwrap(),
            &["rev-parse", "refs/heads/main"],
        )
        .trim(),
    )
    .unwrap();
    let outcome = push_local(
        &local_git,
        &remote,
        &[PushRefSpec {
            src: Some(tip),
            dst: "refs/heads/main".to_owned(),
            ..Default::default()
        }],
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push");
    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);
}

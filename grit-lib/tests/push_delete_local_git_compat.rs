//! Push ref deletion against a local bare remote (Git-compatible receive-pack).

use grit_lib::refs;
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec};

#[test]
fn push_delete_tag_on_local_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .current_dir(&origin)
        .status()
        .unwrap();

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::fs::write(local.join("f"), "x\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "f"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-qm", "c"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["remote", "add", "origin", origin.to_str().unwrap()])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["tag", "-a", "v1", "-m", "t"])
        .current_dir(&local)
        .status()
        .unwrap();

    let local_git = local.join(".git");
    let tag_oid = refs::resolve_ref(&local_git, "refs/tags/v1").unwrap();
    push_local(
        &local_git,
        &origin,
        &[PushRefSpec {
            src: Some(tag_oid),
            dst: "refs/tags/v1".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
            ..Default::default()
        }],
        &PushOptions::default(),
    )
    .expect("push tag");
    assert!(refs::resolve_ref(&origin, "refs/tags/v1").is_ok());

    push_local(
        &local_git,
        &origin,
        &[PushRefSpec {
            src: None,
            dst: "refs/tags/v1".to_owned(),
            force: false,
            delete: true,
            expected_old: None,
            expect_absent: false,
            ..Default::default()
        }],
        &PushOptions::default(),
    )
    .expect("delete tag");
    assert!(refs::resolve_ref(&origin, "refs/tags/v1").is_err());
}

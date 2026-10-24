//! [`create_commit`] identity strings must pass `git fsck --strict`.

use std::process::Command;

use grit_lib::ident::CommitIdentityError;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::staging::stage_worktree_changes;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn create_commit_contact_author_passes_git_fsck_strict() {
    if !git_available() {
        return;
    }

    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let repo = Repository::init(
        root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    std::fs::write(root.join("a.txt"), "hi\n").expect("file");
    stage_worktree_changes(&repo, &[]).expect("stage");

    let req = CommitRequest {
        message: "first\n".into(),
        author: "Doc <doc@example.com>".into(),
        committer: "Doc <doc@example.com>".into(),
        ..Default::default()
    };
    create_commit(&repo, &req, &mut NullProgress).expect("create_commit");

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(root)
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict: stdout={} stderr={}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn create_commit_rejects_malicious_identity_strings() {
    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let repo = Repository::init(
        root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");

    let bad = CommitRequest {
        message: "x\n".into(),
        author: "no email here".into(),
        committer: "Name <a@b>\nInjected: x".into(),
        allow_empty: true,
        ..Default::default()
    };
    let err = create_commit(&repo, &bad, &mut NullProgress).expect_err("invalid identity");
    assert!(
        matches!(
            err,
            grit_lib::error::Error::InvalidIdentity(CommitIdentityError::EmbeddedControl)
                | grit_lib::error::Error::InvalidIdentity(CommitIdentityError::Author(_))
                | grit_lib::error::Error::InvalidIdentity(CommitIdentityError::Unrecognized)
        ),
        "unexpected error: {err}"
    );
}

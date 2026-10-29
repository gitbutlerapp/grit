//! Windows-reserved ref leaf names remain valid loose refs on Unix; grit must not reject them there.

use std::process::Command;

use grit_lib::refs::store::{Expected, FilesRefStore, RawRef, RefStore, RefTransaction, RefUpdate};

use crate::support::{
    assert_fsck_when_git_interop, empty_commit_oid, files_repo, git_interop_available, Backend,
};

#[test]
fn files_store_allows_win32_reserved_refname_on_unix() {
    if !git_interop_available(Backend::Files) {
        eprintln!(
            "skip files_store_allows_win32_reserved_refname_on_unix: git interop unavailable"
        );
        return;
    }

    let repo = files_repo();
    let git_dir = repo.git_dir();
    let oid = empty_commit_oid(&repo);
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/aux".to_owned(),
            new_value: Some(RawRef::Direct(oid)),
            expected: Expected::Missing,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    assert!(git_dir.join("refs/heads/aux").is_file());

    let rev = Command::new("git")
        .env("GIT_DIR", &git_dir)
        .args(["rev-parse", "refs/heads/aux"])
        .output()
        .expect("git rev-parse");
    assert!(
        rev.status.success(),
        "git rev-parse: {}",
        String::from_utf8_lossy(&rev.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&rev.stdout).trim(),
        oid.to_hex().to_string()
    );

    assert_fsck_when_git_interop(&repo);
}

//! Path-limited revision walks must not treat non-tree commit `tree` headers as trees.

use grit_lib::progress::{CancelToken, NullProgress};

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{serialize_commit, serialize_tree, CommitData, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::RevListOptions;

#[test]
fn pathspec_log_skips_commit_when_tree_header_points_at_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::new().with_initial_branch("main"))
        .expect("init");
    let blob_oid = repo
        .objects()
        .write(ObjectKind::Blob, b"tracked\n")
        .expect("blob");
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"tracked.txt".to_vec(),
        oid: blob_oid,
    }]);
    let tree_as_blob = repo
        .objects()
        .write(ObjectKind::Blob, &tree_body)
        .expect("tree bytes stored as blob");
    let commit = CommitData::new(
        tree_as_blob,
        vec![],
        "T <t@example.com> 1 +0000",
        "T <t@example.com> 1 +0000",
        "bad tree link\n",
    );
    let commit_oid = repo
        .objects()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    grit_test_support::refs::set_ref(&repo, "refs/heads/main", &commit_oid).expect("update ref");

    let git_log = Command::new("git")
        .current_dir(dir.path())
        .args(["log", "--oneline", "--", "tracked.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git log");
    assert!(
        !git_log.status.success(),
        "git should reject non-tree commit tree header: {}",
        String::from_utf8_lossy(&git_log.stderr)
    );

    let open = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let opts = RevListOptions {
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = open.revwalk().rev_list(
        &["HEAD".to_owned()],
        &[],
        &opts,
        &mut NullProgress,
        &CancelToken::never(),
    );
    match result {
        Err(_) => {}
        Ok(out) => assert!(
            out.commits.is_empty(),
            "path-limited log must not list commit with blob tree header, got {:?}",
            out.commits
        ),
    }
}

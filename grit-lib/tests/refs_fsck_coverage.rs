//! Basic `refs_fsck` integration for files-backend repositories.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::write_ref;
use grit_lib::refs_fsck::{format_refs_fsck_line, refs_fsck, RefsFsckIssue};
use grit_lib::repo::{init_repository, Repository};
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn refs_fsck_reports_bad_symref_and_formats_line() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    let repo = Repository::open(&git_dir, Some(dir.path())).expect("open");
    let config = ConfigSet::load_repo_local_only(&git_dir).expect("config");
    let odb = repo.odb.clone();

    std::fs::write(
        git_dir.join("refs/heads/bad-sym"),
        "ref: refs/heads/missing-target\n",
    )
    .expect("symref");

    write_ref(&git_dir, "refs/heads/good", &oid(1)).expect("good ref");

    let issues = refs_fsck(&repo, &odb, &config, false).expect("fsck");
    assert!(
        issues
            .iter()
            .any(|i| i.path.contains("bad-sym") || !i.detail.is_empty()),
        "expected symref issue, got {issues:?}"
    );

    let sample = RefsFsckIssue {
        severity: grit_lib::refs_fsck::RefsFsckSeverity::Error,
        path: "refs/heads/sample".into(),
        msg_id: "badRefName".into(),
        detail: "demo".into(),
    };
    let line = format_refs_fsck_line(&sample);
    assert!(line.contains("badRefName"));
}

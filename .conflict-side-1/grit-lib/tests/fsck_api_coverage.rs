//! Coverage touch for public repository fsck types and helpers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::maintenance::fsck::FsckError;
use grit_lib::maintenance::repository_fsck::{
    format_fsck_line, issue_msg_id, issue_severity, FsckIssue, FsckOptions, FsckSeverity,
};
use grit_lib::objects::ObjectId;
use grit_lib::refs::refs_fsck::RefsFsckIssue;
use std::path::PathBuf;

#[test]
fn fsck_options_constructors_and_defaults() {
    let d = FsckOptions::default();
    assert!(d.check_dangling);
    assert!(d.check_connectivity);
    assert!(!d.connectivity_only);
    let full = FsckOptions::full_repository(true);
    assert!(full.strict);
    let conn = FsckOptions::connectivity_and_refs(false);
    assert!(conn.connectivity_only);
    assert!(conn.check_connectivity);
    assert!(!conn.check_dangling);
    assert!(
        !FsckOptions::full_repository(false)
            .with_check_dangling(false)
            .check_dangling
    );
}

#[test]
fn issue_msg_id_and_severity_and_format() {
    let config = ConfigSet::default();
    let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").expect("oid");
    let issue = FsckIssue::MissingObject {
        oid,
        referenced_by: oid,
    };
    assert_eq!(issue_msg_id(&issue), "missingObject");
    assert_eq!(issue_severity(&issue, &config, false), FsckSeverity::Error);
    let line = format_fsck_line(&issue, &config, false);
    assert!(line.contains("missingObject"));

    let bad_ref = FsckIssue::BadRef(RefsFsckIssue {
        severity: grit_lib::refs::refs_fsck::RefsFsckSeverity::Warning,
        path: "refs/heads/x".into(),
        msg_id: "badRefName",
        detail: "demo".into(),
    });
    assert_eq!(issue_msg_id(&bad_ref), "badRefName");
    assert_eq!(issue_severity(&bad_ref, &config, true), FsckSeverity::Error);

    let _ = FsckIssue::CorruptLoose {
        oid,
        path: PathBuf::from("/tmp/x"),
    };
    let _ = FsckIssue::HashMismatch {
        oid,
        path: PathBuf::from("/tmp/x"),
    };
    let bad_tree = FsckIssue::BadObject {
        oid,
        error: FsckError::new("badTree", "x"),
    };
    assert_eq!(
        issue_severity(&bad_tree, &config, false),
        FsckSeverity::Error
    );

    let missing_tagger = FsckIssue::BadObject {
        oid,
        error: FsckError::new("missingTaggerEntry", "expected tagger line"),
    };
    assert_eq!(
        issue_severity(&missing_tagger, &config, false),
        FsckSeverity::Warning
    );
    assert_eq!(
        issue_severity(&missing_tagger, &config, true),
        FsckSeverity::Warning
    );

    let has_dot = FsckIssue::BadObject {
        oid,
        error: FsckError::new("hasDot", "dot entry"),
    };
    assert_eq!(
        issue_severity(&has_dot, &config, false),
        FsckSeverity::Warning
    );
    assert_eq!(issue_severity(&has_dot, &config, true), FsckSeverity::Error);
    let _ = FsckIssue::PackChecksum {
        pack: PathBuf::from("pack.pack"),
    };
    let _ = FsckIssue::IdxMismatch {
        pack: PathBuf::from("pack.pack"),
    };
    let _ = FsckIssue::BrokenLink {
        tree: oid,
        name: "a".into(),
        mode: 0o100644,
        target: oid,
    };
    let _ = FsckIssue::MidxInvalid {
        details: vec!["bad".into()],
    };
    let _ = FsckIssue::CommitGraphInvalid {
        detail: "bad".into(),
    };
}

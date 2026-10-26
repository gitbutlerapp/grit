//! Regression tests for the `serde-models` feature.

#![cfg(feature = "serde-models")]

use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::log::{CommitRefDecoration, DecorationKind, LoadedCommit};
use grit_lib::porcelain::status::StatusModel;
use grit_lib::state::HeadState;
use serde_json::Value;

#[test]
fn porcelain_dtos_serialize_to_stable_json_shapes() {
    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let oid_json = serde_json::to_value(oid).expect("object id");
    assert_eq!(
        oid_json,
        Value::String("4b825dc642cb6eb9a060e54bf8d69288fbee4904".into())
    );

    let change_id = ChangeId::from_bytes(&[0u8; 16]);
    let change_json = serde_json::to_value(&change_id).expect("change id");
    assert!(change_json.is_string());

    let decoration = CommitRefDecoration {
        name: "main".into(),
        kind: DecorationKind::Branch,
    };
    let deco_json = serde_json::to_value(&decoration).expect("decoration");
    assert_eq!(deco_json["kind"], Value::String("branch".into()));

    let loaded = LoadedCommit {
        oid,
        parents: vec![],
        author: "A".into(),
        committer: "C".into(),
        message: "m".into(),
        refs: vec![decoration],
        change_id,
        change_id_stored: false,
    };
    let _ = serde_json::to_string(&loaded).expect("LoadedCommit");

    let status = StatusModel {
        head: HeadState::Detached { oid },
        head_tree: None,
        state: grit_lib::state::WtStatusState::default(),
        index: grit_lib::index::Index::new(),
        staged: vec![],
        unstaged: vec![],
        untracked: vec![],
        ignored: vec![],
        conflicts: vec![],
        conflict_details: vec![],
        operation: None,
        stash_count: 0,
        index_sparse_on_disk: false,
        sparse_directory_prefixes: vec![],
    };
    let _ = serde_json::to_string(&status).expect("StatusModel");
}

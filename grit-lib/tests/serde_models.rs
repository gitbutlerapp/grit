//! Regression tests for the `serde-models` feature.

#![cfg(feature = "serde-models")]

use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind, RenamedPaths};
use grit_lib::porcelain::log::{CommitRefDecoration, DecorationKind, LoadedCommit};
use grit_lib::porcelain::status::StatusModel;
use grit_lib::state::HeadState;
use serde_json::Value;

fn assert_serialize<T: serde::Serialize + ?Sized>() {}

/// Every result model a front end renders must serialize without a hand-written DTO.
#[test]
fn result_models_implement_serialize() {
    assert_serialize::<grit_lib::porcelain::commit::CommitOutcome>();
    assert_serialize::<grit_lib::porcelain::switch::SwitchOutcome>();
    assert_serialize::<grit_lib::porcelain::reset::ResetOutcome>();
    assert_serialize::<grit_lib::porcelain::reset::ResetPreview>();
    assert_serialize::<grit_lib::porcelain::branch::BranchList>();
    assert_serialize::<grit_lib::remote::transfer::FetchOutcome>();
    assert_serialize::<grit_lib::remote::transfer::PushOutcome>();
    assert_serialize::<grit_lib::porcelain::stash::StashEntry>();
    assert_serialize::<grit_lib::porcelain::stash::StashApplyOutcome>();
    assert_serialize::<grit_lib::ident::Signature>();
    assert_serialize::<grit_lib::revwalk::history::RewritePlan>();
    assert_serialize::<grit_lib::revwalk::history::RebaseOutcome>();
    assert_serialize::<grit_lib::operation::Operation>();
    assert_serialize::<grit_lib::operation::Conflict>();
    assert_serialize::<grit_lib::error::Error>();
    assert_serialize::<grit_lib::porcelain::status::StatusModel>();
    assert_serialize::<grit_lib::porcelain::log::LoadedCommit>();
    assert_serialize::<grit_lib::diff::DiffEntry>();
}

fn sample_signature() -> grit_lib::ident::Signature {
    "Ada Lovelace <ada@example.com> 1700000000 -0800"
        .parse()
        .expect("signature")
}

#[test]
fn signature_serializes_name_email_and_rfc3339_date() {
    let json = serde_json::to_value(sample_signature()).expect("signature");
    assert_eq!(
        json,
        serde_json::json!({
            "name": "Ada Lovelace",
            "email": "ada@example.com",
            "date": "2023-11-14T14:13:20-08:00",
        })
    );
}

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
        author: sample_signature(),
        committer: sample_signature(),
        message: "m".into(),
        refs: vec![decoration],
        change_id,
        change_id_stored: false,
    };
    let loaded_json = serde_json::to_value(&loaded).expect("LoadedCommit");
    assert_eq!(loaded_json["author"]["email"], "ada@example.com");
    assert_eq!(
        loaded_json["committer"]["date"],
        "2023-11-14T14:13:20-08:00"
    );

    let state = grit_lib::state::WtStatusState {
        merge_in_progress: true,
        ..Default::default()
    };
    let operation = grit_lib::state::status_operation(&state, true);
    let status = StatusModel {
        head: HeadState::Detached { oid },
        head_tree: None,
        state,
        index: grit_lib::index::Index::new(),
        staged: vec![],
        unstaged: vec![],
        untracked: vec![],
        embedded_repositories: vec![],
        ignored: vec![],
        conflicts: vec!["a.txt".into()],
        conflict_details: vec![Conflict {
            path: "a.txt".into(),
            kind: ConflictKind::BothModified,
        }],
        operation,
        stash_count: 0,
        index_sparse_on_disk: false,
        sparse_directory_prefixes: vec![],
        interrupted_rewrite: Some(
            grit_lib::porcelain::status::InterruptedRewrite::FinishedCheckout,
        ),
    };
    let status_json = serde_json::to_value(&status).expect("StatusModel");
    assert_eq!(status_json["state"]["merge_in_progress"], true);
    assert_eq!(status_json["conflict_details"][0]["path"], "a.txt");
    assert_eq!(status_json["operation"]["kind"], "merge");
    assert!(status_json["operation"]["next_steps"]
        .as_array()
        .is_some_and(|steps| !steps.is_empty()));
    assert_eq!(
        status_json["conflict_details"],
        serde_json::json!([{"path": "a.txt", "kind": "both_modified"}])
    );

    let rename = Conflict {
        path: "a".into(),
        kind: ConflictKind::RenameRename(Box::new(RenamedPaths {
            base_path: "a".into(),
            ours_path: "b".into(),
            theirs_path: "c".into(),
        })),
    };
    assert_eq!(
        serde_json::to_value(&rename).expect("rename conflict"),
        serde_json::json!({
            "path": "a",
            "kind": "rename_rename",
            "base_path": "a",
            "ours_path": "b",
            "theirs_path": "c",
        })
    );
    for kind in [
        ConflictKind::BothModified,
        ConflictKind::BothAdded,
        ConflictKind::BothDeleted,
        ConflictKind::AddedByUs,
        ConflictKind::AddedByThem,
        ConflictKind::DeletedByUs,
        ConflictKind::DeletedByThem,
    ] {
        let json = serde_json::to_value(Conflict {
            path: "p".into(),
            kind: kind.clone(),
        })
        .expect("conflict");
        assert_eq!(json["kind"], kind.as_str());
    }
}

#[test]
fn diff_entry_modes_serialize_as_octal_strings() {
    use grit_lib::diff::{DiffEntry, DiffStatus};
    use grit_lib::objects::FileMode;

    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let entry = DiffEntry {
        status: DiffStatus::TypeChanged,
        old_path: Some("link".into()),
        new_path: Some("link".into()),
        old_mode: FileMode::REGULAR,
        new_mode: FileMode::SYMLINK,
        old_oid: oid,
        new_oid: oid,
        score: None,
    };
    let json = serde_json::to_value(&entry).expect("DiffEntry");
    assert_eq!(json["old_mode"], "100644");
    assert_eq!(json["new_mode"], "120000");
    assert_eq!(json["status"], "type_changed");
}

#[test]
fn errors_serialize_with_kind_and_message() {
    let err = grit_lib::error::Error::NothingToCommit;
    let json = serde_json::to_value(&err).expect("Error");
    assert_eq!(json["kind"], "nothing_to_commit");
    assert_eq!(json["message"], err.to_string());

    let err = grit_lib::error::Error::Cancelled;
    assert_eq!(err.kind_name(), "cancelled");

    let err = grit_lib::error::Error::LockHeld {
        path: std::path::PathBuf::from(".git/index.lock"),
        holder: Some(grit_lib::lockfile::LockHolder {
            pid: 42,
            running: true,
        }),
    };
    let json = serde_json::to_value(&err).expect("LockHeld");
    assert_eq!(json["kind"], "lock_held");
    assert_eq!(json["path"], ".git/index.lock");
    assert_eq!(json["holder"]["pid"], 42);
    assert_eq!(json["holder"]["running"], true);

    let err = grit_lib::error::Error::StashCouldNotRestoreUntracked {
        path: "u.txt".into(),
    };
    let json = serde_json::to_value(&err).expect("stash untracked");
    assert_eq!(json["path"], "u.txt");
}

#[test]
fn stash_and_branch_outcomes_serialize() {
    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let entry = grit_lib::porcelain::stash::StashEntry {
        index: 0,
        oid,
        message: "WIP on main".into(),
    };
    let json = serde_json::to_value(&entry).expect("StashEntry");
    assert_eq!(json["oid"], oid.to_hex());

    let list = grit_lib::porcelain::branch::BranchList {
        current: Some("main".into()),
        branches: vec![grit_lib::porcelain::branch::LocalBranch {
            name: "main".into(),
            oid,
            current: true,
        }],
    };
    let json = serde_json::to_value(&list).expect("BranchList");
    assert_eq!(json["branches"][0]["name"], "main");
}

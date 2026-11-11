//! JSON shapes of `serde-models` output structs that callers outside the crate cannot build.
//!
//! `StatusModel`, `LoadedCommit` and `BranchSummary` are `#[non_exhaustive]` and have no
//! public constructor, so these checks build them with struct literals here instead of in
//! `tests/serde_models.rs`.

use crate::branch_inventory::BranchSummary;
use crate::change_id::ChangeId;
use crate::ident::Signature;
use crate::index::Index;
use crate::objects::ObjectId;
use crate::operation::{Conflict, ConflictKind};
use crate::porcelain::log::{CommitRefDecoration, DecorationKind, LoadedCommit};
use crate::porcelain::status::{InterruptedRewrite, StatusModel};
use crate::state::{status_operation, HeadState, WtStatusState};
use serde_json::Value;
use time::OffsetDateTime;

fn oid() -> ObjectId {
    ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid")
}

fn sample_signature() -> Signature {
    "Ada Lovelace <ada@example.com> 1700000000 -0800"
        .parse()
        .expect("signature")
}

#[test]
fn loaded_commit_serializes_signatures_and_decorations() {
    let decoration = CommitRefDecoration::new("main", DecorationKind::Branch);
    let deco_json = serde_json::to_value(&decoration).expect("decoration");
    assert_eq!(deco_json["kind"], Value::String("branch".into()));

    let loaded = LoadedCommit {
        oid: oid(),
        parents: vec![],
        author: sample_signature(),
        committer: sample_signature(),
        message: "m".into(),
        refs: vec![decoration],
        change_id: ChangeId::from_bytes(&[0u8; 16]),
        change_id_stored: false,
    };
    let loaded_json = serde_json::to_value(&loaded).expect("LoadedCommit");
    assert_eq!(loaded_json["author"]["email"], "ada@example.com");
    assert_eq!(
        loaded_json["committer"]["date"],
        "2023-11-14T14:13:20-08:00"
    );
}

#[test]
fn status_model_serializes_state_operation_and_conflicts() {
    let state = WtStatusState {
        merge_in_progress: true,
        ..Default::default()
    };
    let operation = status_operation(&state, true);
    let status = StatusModel {
        head: HeadState::Detached { oid: oid() },
        head_tree: None,
        state,
        index: Index::new(),
        staged: vec![],
        unstaged: vec![],
        untracked: vec![],
        embedded_repositories: vec![],
        ignored: vec![],
        conflicts: vec!["a.txt".into()],
        conflict_details: vec![Conflict::new("a.txt", ConflictKind::BothModified)],
        operation,
        stash_count: 0,
        interrupted_rewrite: Some(InterruptedRewrite::FinishedCheckout),
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
}

#[test]
fn branch_summary_serializes_committer_time_as_rfc3339() {
    let summary = BranchSummary {
        name: "main".into(),
        full_ref: "refs/heads/main".into(),
        is_remote: false,
        current: true,
        oid: oid(),
        subject: Some("init".into()),
        committer_time: Some(OffsetDateTime::UNIX_EPOCH),
        upstream: None,
        worktree: None,
    };
    let json = serde_json::to_value(&summary).expect("BranchSummary");
    let date = json["committer_time"].as_str().expect("date");
    assert!(
        date == "1970-01-01T00:00:00+00:00" || date == "1970-01-01T00:00:00Z",
        "{date}"
    );
}

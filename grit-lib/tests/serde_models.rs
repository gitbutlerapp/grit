//! Regression tests for the `serde-models` feature.

#![cfg(feature = "serde-models")]

use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind, RenamedPaths};
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
    assert_serialize::<grit_lib::stash::StashEntry>();
    assert_serialize::<grit_lib::stash::StashApplyOutcome>();
    assert_serialize::<grit_lib::ident::Signature>();
    assert_serialize::<grit_lib::revwalk::history::RewritePlan>();
    assert_serialize::<grit_lib::revwalk::history::RebaseOutcome>();
    assert_serialize::<grit_lib::state::Operation>();
    assert_serialize::<grit_lib::operation::Conflict>();
    assert_serialize::<grit_lib::error::Error>();
    assert_serialize::<grit_lib::porcelain::status::StatusModel>();
    assert_serialize::<grit_lib::porcelain::log::LoadedCommit>();
    assert_serialize::<grit_lib::diff::DiffEntry>();
    assert_serialize::<grit_lib::porcelain::tag::TagSummary>();
    assert_serialize::<grit_lib::porcelain::tag::TagList>();
    assert_serialize::<grit_lib::blame::BlameResult>();
    assert_serialize::<grit_lib::blame::BlameHunk>();
    assert_serialize::<grit_lib::blame::BlameCommitInfo>();
    assert_serialize::<grit_lib::blame::BlameAttributedLine>();
    assert_serialize::<grit_lib::worktree::WorktreeEntry>();
    assert_serialize::<grit_lib::refs::store::RefName>();
    assert_serialize::<grit_lib::objects::ObjectKind>();
    assert_serialize::<grit_lib::porcelain::workspace_snapshot::WorkspaceSnapshot>();
    assert_serialize::<grit_lib::revwalk::DescribedCommit>();
    assert_serialize::<grit_lib::porcelain::merge::IntegrateBranchOutcome>();
    assert_serialize::<grit_lib::porcelain::pull::PullOutcome>();
    assert_serialize::<grit_lib::porcelain::pull::PullResult>();
    assert_serialize::<grit_lib::porcelain::pull::PullCheck>();
    assert_serialize::<grit_lib::remote::clone::CloneOutcome>();
    assert_serialize::<grit_lib::porcelain::checkout::CheckoutOutcome>();
    assert_serialize::<grit_lib::porcelain::clean::CleanOutcome>();
    assert_serialize::<grit_lib::config::ConfigEntry>();
    assert_serialize::<grit_lib::porcelain::tag::CreateTagOutcome>();
    assert_serialize::<grit_lib::diff::DiffStat>();
    assert_serialize::<grit_lib::diff::FileDiff>();
    assert_serialize::<grit_lib::maintenance::GcStats>();
    assert_serialize::<grit_lib::index::IndexEntry>();
    assert_serialize::<grit_lib::porcelain::paths::MoveOutcome>();
    assert_serialize::<grit_lib::repo_handles::NoteListing>();
    assert_serialize::<grit_lib::objects::ObjectInfo>();
    assert_serialize::<grit_lib::porcelain::paths::RemoveOutcome>();
    assert_serialize::<grit_lib::porcelain::replay::ReplayOutcome>();
    assert_serialize::<grit_lib::porcelain::restore::RestoreOutcome>();
    assert_serialize::<grit_lib::revwalk::rev_list::RevListResult>();
    assert_serialize::<grit_lib::porcelain::add::StageOutcome>();
    assert_serialize::<grit_lib::stash::StashPushResult>();
    assert_serialize::<grit_lib::refs::branch_tracking::UpstreamStatus>();
    assert_serialize::<grit_lib::objects::TreeEntry>();
    assert_serialize::<grit_lib::bundle::UnbundleOutcome>();
    assert_serialize::<grit_lib::revwalk::history::UndoOutcome>();
    assert_serialize::<grit_lib::remote::serve::UploadOutcome>();
    assert_serialize::<grit_lib::refs::branch_tracking::BranchSummary>();
    assert_serialize::<grit_lib::refs::branch_tracking::TargetComparison>();
    assert_serialize::<grit_lib::refs::listing::RefListing>();
    assert_serialize::<grit_lib::refs::reflog::ReflogEntry>();
    assert_serialize::<grit_lib::remote::serve::ReceiveOutcome>();
    assert_serialize::<grit_lib::revwalk::history::RebasePlan>();
}

fn assert_deserialize<T: serde::de::DeserializeOwned>() {}

#[test]
fn workspace_snapshot_implement_deserialize() {
    assert_deserialize::<grit_lib::porcelain::workspace_snapshot::WorkspaceSnapshot>();
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

    let rename = Conflict::new(
        "a",
        ConflictKind::RenameRename(Box::new(RenamedPaths {
            base_path: "a".into(),
            ours_path: "b".into(),
            theirs_path: "c".into(),
        })),
    );
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
        let json = serde_json::to_value(Conflict::new("p", kind.clone())).expect("conflict");
        assert_eq!(json["kind"], kind.as_str());
    }
}

#[test]
fn diff_entry_modes_serialize_as_octal_strings() {
    use grit_lib::diff::{DiffEntry, DiffStatus};
    use grit_lib::objects::FileMode;

    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let entry = DiffEntry::new(
        DiffStatus::TypeChanged,
        Some("link".into()),
        Some("link".into()),
        FileMode::REGULAR,
        FileMode::SYMLINK,
        oid,
        oid,
    );
    let json = serde_json::to_value(&entry).expect("DiffEntry");
    assert_eq!(json["old_mode"], "100644");
    assert_eq!(json["new_mode"], "120000");
    assert_eq!(json["status"], "type_changed");
}

#[test]
fn errors_serialize_with_kind_and_message() {
    let err = grit_lib::error::Error::Commit(grit_lib::porcelain::CommitError::NothingToCommit);
    let json = serde_json::to_value(&err).expect("Error");
    assert_eq!(json["kind"], "nothing_to_commit");
    assert_eq!(json["message"], err.to_string());
    assert_eq!(json["area"], "commit");

    let err = grit_lib::error::Error::Cancelled;
    assert_eq!(err.kind_name(), "cancelled");

    let err = grit_lib::error::Error::LockHeld(grit_lib::error::LockHeldError::new(
        std::path::PathBuf::from(".git/index.lock"),
        Some(grit_lib::lockfile::LockHolder::new(42, true)),
    ));
    let json = serde_json::to_value(&err).expect("LockHeld");
    assert_eq!(json["kind"], "lock_held");
    assert_eq!(json["area"], "lock_held");
    assert_eq!(json["path"], ".git/index.lock");
    assert_eq!(json["holder"]["pid"], 42);
    assert_eq!(json["holder"]["running"], true);

    let err = grit_lib::stash::StashError::CouldNotRestoreUntracked {
        path: "u.txt".into(),
    };
    let json = serde_json::to_value(&err).expect("stash untracked");
    assert_eq!(json["kind"], "stash_could_not_restore_untracked");
    assert_eq!(json["path"], "u.txt");
}

#[test]
fn commit_author_info_serializes_rfc3339_date() {
    let info = grit_lib::porcelain::commit::CommitAuthorInfo::new(
        "Ada Lovelace",
        "ada@example.com",
        "1700000000 -0800",
    );
    let json = serde_json::to_value(&info).expect("CommitAuthorInfo");
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
fn index_entry_json_shape() {
    use grit_lib::index::IndexEntry;
    use grit_lib::objects::{FileMode, ObjectId};

    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let entry = IndexEntry::new(
        b"src/main.rs".to_vec(),
        oid,
        FileMode::new(FileMode::REGULAR.bits()),
    );
    let json = serde_json::to_value(&entry).expect("IndexEntry");
    assert_eq!(json["path"], "src/main.rs");
    assert_eq!(json["mode"], "100644");
}

fn stash_and_branch_outcomes_serialize() {
    let oid = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
    let entry = grit_lib::stash::StashEntry::new(0, oid, "WIP on main");
    let json = serde_json::to_value(&entry).expect("StashEntry");
    assert_eq!(json["oid"], oid.to_hex());

    let list = grit_lib::porcelain::branch::BranchList::new(
        Some("main".into()),
        vec![grit_lib::porcelain::branch::LocalBranch::new(
            "main", oid, true,
        )],
    );
    let json = serde_json::to_value(&list).expect("BranchList");
    assert_eq!(json["branches"][0]["name"], "main");
}

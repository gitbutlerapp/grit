//! Coverage for [`grit_lib::porcelain::status::assemble_status_summary`].

use grit_lib::operation::{ConflictKind, ConflictKindPhrase};
use grit_lib::porcelain::status::{
    assemble_status_summary, StatusGuidance, StatusOptions, StatusSummaryOptions,
};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::state::InProgressOperation;

#[test]
fn assemble_status_summary_on_clean_repo() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::from)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    let model = repo.status().run(
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    let summary = assemble_status_summary(&repo, model, StatusSummaryOptions::default())?;
    assert!(summary.conflict_groups.is_empty());
    assert!(summary.in_progress.is_empty());
    assert_ne!(summary.guidance, StatusGuidance::ContinueMerge);
    Ok(())
}

#[test]
fn conflict_kind_phrase_and_operation_json_kind() {
    assert_eq!(
        ConflictKind::BothModified.phrase(),
        ConflictKindPhrase::BothModified
    );
    assert_eq!(InProgressOperation::Merge.status_json_kind(), "merge");
}

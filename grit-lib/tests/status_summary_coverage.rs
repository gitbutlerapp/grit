//! Coverage for [`grit_lib::status::Status::assemble_summary`].

use grit_lib::operation::{ConflictKind, ConflictKindPhrase};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::status::{StatusGuidance, StatusOptions, StatusSummaryOptions};

#[test]
fn assemble_summary_on_clean_repo() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::from)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    let status = repo.status();
    let model = status.run(
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    let summary = status.assemble_summary(model, StatusSummaryOptions::default())?;
    assert!(summary.conflict_groups.is_empty());
    assert!(summary.in_progress.is_empty());
    assert_ne!(summary.guidance, StatusGuidance::ContinueMerge);
    Ok(())
}

#[test]
fn conflict_kind_phrase_classification() {
    assert_eq!(
        ConflictKind::BothModified.phrase(),
        ConflictKindPhrase::BothModified
    );
    assert_eq!(
        ConflictKind::BothAdded.phrase(),
        ConflictKindPhrase::BothAdded
    );
    assert_eq!(
        ConflictKind::DirectoryRenameBlocked(Box::new(grit_lib::operation::BlockedMove {
            sources: vec!["a".to_owned()],
            existing: true,
        }))
        .phrase(),
        ConflictKindPhrase::DirectoryRenameBlocked
    );
}

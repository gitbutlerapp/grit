//! `grit status` human and Markdown rendering helpers.

use grit_lib::operation::ConflictKindPhrase;

/// Human-readable heading for a grouped conflict section.
#[must_use]
pub fn conflict_kind_heading(phrase: ConflictKindPhrase) -> &'static str {
    match phrase {
        ConflictKindPhrase::BothModified => "both modified",
        ConflictKindPhrase::BothAdded => "both added",
        ConflictKindPhrase::BothDeleted => "both deleted",
        ConflictKindPhrase::AddedByUs => "added by us",
        ConflictKindPhrase::AddedByThem => "added by them",
        ConflictKindPhrase::DeletedByUs => "deleted by us",
        ConflictKindPhrase::DeletedByThem => "deleted by them",
        ConflictKindPhrase::RenameRename => "renamed differently",
        ConflictKindPhrase::DirectoryRenameMoved => "moved with its directory",
        ConflictKindPhrase::DirectoryRenameSplit => "directory split up",
        ConflictKindPhrase::DirectoryRenameBlocked => "directory move blocked",
        _ => "unmerged",
    }
}

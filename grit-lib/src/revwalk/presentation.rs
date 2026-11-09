//! CLI-agnostic presentation choices for rev-list style output.

use crate::revwalk::CommitExtent;

/// How `grit revs` (and similar listers) retain commit bytes while walking.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RevListPresentationOptions;

impl RevListPresentationOptions {
    /// Commit object extent to use when listing for the given JSON output mode.
    ///
    /// When `full_json` is true, whole commits are retained; otherwise only through the subject line.
    #[must_use]
    pub fn commit_extent_for_full_json(self, full_json: bool) -> CommitExtent {
        if full_json {
            CommitExtent::Whole
        } else {
            CommitExtent::ThroughSubject
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_extent_whole_for_json() {
        let opts = RevListPresentationOptions;
        assert_eq!(opts.commit_extent_for_full_json(true), CommitExtent::Whole);
    }

    #[test]
    fn commit_extent_through_subject_without_json() {
        let opts = RevListPresentationOptions;
        assert_eq!(
            opts.commit_extent_for_full_json(false),
            CommitExtent::ThroughSubject
        );
    }
}

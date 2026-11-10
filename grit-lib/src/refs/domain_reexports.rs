//! Canonical nested paths for ref-adjacent modules (flat [`crate::reflog`] paths remain valid).

pub mod reflog {
    //! reflog API (re-exported at this path).

    pub use crate::reflog::{
        parse_reflog_ref, reflog_identity_timestamp, MessageMatcher, ReflogEntry,
        ReflogExpireActionKind, ReflogSelector,
    };
}
pub mod refspec {
    //! refspec API (re-exported at this path).
    pub use crate::refspec::parse_push_refspec;
    pub use crate::refspec::RefspecError;
    pub use crate::refspec::RefspecItem;
}
pub mod reftable {
    //! reftable API (re-exported at this path).

    pub use crate::reftable::{
        dump_reftable_blocks, LogRecord, RefRecord, RefValue, ReftableError, ReftableField,
        ReftableReader, ReftableStack, ReftableTransactionUpdate, ReftableWriteOptions,
        ReftableWriter, WriteOptions,
    };
}
pub mod check_ref_format {
    //! check_ref_format API (re-exported at this path).

    pub use crate::check_ref_format::{RefName, RefNameError, RefNamePolicy, RefNameRef};
}
pub mod ref_namespace {
    //! ref_namespace API (re-exported at this path).
    pub use crate::ref_namespace::{
        logical_ref_name_from_storage, logical_ref_name_from_storage_env, storage_ref_name,
    };
}
pub mod hide_refs {
    //! hide_refs API (re-exported at this path).

    pub use crate::hide_refs::{hide_ref_patterns_receive, hide_ref_patterns_uploadpack};
}
pub mod ref_storage {
    //! ref_storage API (re-exported at this path).

    pub use crate::ref_storage::RefStorageFormat;
}
pub mod branch_tracking {
    //! branch_tracking API (re-exported at this path).

    pub use crate::branch_tracking::BoundedCommitSide;
    pub use crate::branch_tracking::{
        status_ahead_behind_enabled, BaseSource, BranchDetail, BranchListQuery, BranchScope,
        BranchStanding, BranchSummary, ComparisonBase, Standing, StandingHeader, StandingOptions,
        StatusHeader, StatusIntegrationAction, StatusIntegrationHint, TargetComparison,
        TargetComparisonOptions, TargetSpec, TipComparison, TrackedComparison, UpstreamStatus,
        DEFAULT_TARGET_CANDIDATES,
    };
}
pub mod refs_fsck {
    //! refs_fsck API (re-exported at this path).
    pub use crate::refs_fsck::{format_refs_fsck_line, refs_fsck, RefsFsckIssue, RefsFsckSeverity};
}

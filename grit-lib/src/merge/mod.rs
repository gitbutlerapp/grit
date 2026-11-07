//! Merge, pick, revert, replay and rerere for one [`Repository`].
//!
//! Obtain [`Merge`] from [`Repository::merge`](crate::repo::Repository::merge). Branch
//! integration, conflict prediction, merge `--continue` / `--abort`, cherry-pick and revert
//! sequences, and rerere all live on that handle rather than as free functions taking
//! [`Repository`].
//!
//! Library guide: [embedding](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::merge::{MergeOptions, MergePredictionOptions};
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = repo.merge();
//! # Ok(())
//! # }
//! ```

mod handle;
mod rerere;

pub use crate::rerere::{RerereAutoupdate, RerereEvent, RerereEventKind};
pub use handle::{BranchIntegrateRequest, Merge};
pub use rerere::MergeRerere;

pub use crate::porcelain::merge::{
    ContinueOptions, ContinueOutcome, FastForwardPolicy, IntegrateBranchOptions,
    IntegrateBranchOutcome, MergeCommitMode, MergeDryRunPlan, MergeOptions, MergePrediction,
    MergePredictionOptions, MergeSource, MergeSourceKind, MergeTreeStage, WrittenMergeTree,
};
pub use crate::porcelain::merge_error::MergeError;
pub use crate::porcelain::replay::{ReplayDirection, ReplayOutcome, ReplayRequest};
pub use crate::porcelain::replay_sequence::{
    ReplayAbortOutcome, ReplayCreated, ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
pub mod merge_diff {
    //! Combined diff content for merge commits.
    pub use crate::merge_diff::*;
}
pub mod merge_file {
    //! Single-file three-way merge.
    pub use crate::merge_file::*;
}
pub mod merge_trees {
    //! Tree-level three-way merge engine.
    pub use crate::merge_trees::*;
}

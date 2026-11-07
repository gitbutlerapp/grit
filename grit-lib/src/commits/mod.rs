//! Create and plan commits on a repository ([`Commits`] from [`Repository::commits`](crate::repo::Repository::commits)).

mod handle;

pub use handle::{CommitOptions, Commits};

pub use crate::porcelain::commit::{
    CommitAuthorInfo, CommitFile, CommitOutcome, CommitPatchInput, CommitPlan, CommitRequest,
    CommitScope, HookPolicy,
};

pub use crate::porcelain::CommitError;

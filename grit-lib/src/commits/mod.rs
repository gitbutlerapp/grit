//! Create and plan commits on a repository ([`Commits`] from [`Repository::commits`](crate::repo::Repository::commits)).
//!
//! ```rust
//! use grit_lib::commits::{CommitOptions, CommitRequest};
//! use grit_lib::porcelain::add::StageOptions;
//! use grit_lib::porcelain::commit::CommitScope;
//! use grit_lib::progress::{CancelToken, NullProgress};
//! use grit_lib::repo::{InitOptions, Repository};
//!
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! std::fs::write(dir.path().join("README.md"), b"hello\n")?;
//! repo.index().stage(
//!     &StageOptions::default(),
//!     &mut NullProgress,
//!     &CancelToken::never(),
//! )?;
//! let ident = "User <u@example.com> 1700000000 +0000";
//! let mut req = CommitRequest::new("init");
//! req.author = Some(ident.parse()?);
//! req.committer = Some(ident.parse()?);
//! let options = CommitOptions::new(req, CommitScope::Staged);
//! let _plan = repo.commits().plan(&options, &mut NullProgress, &CancelToken::never())?;
//! let _outcome = repo.commits().create(&options, &mut NullProgress, &CancelToken::never())?;
//! # Ok(())
//! # }
//! ```

mod handle;

pub use handle::{CommitOptions, Commits};

pub use crate::porcelain::commit::{
    CommitAuthorInfo, CommitFile, CommitOutcome, CommitPatchInput, CommitPlan, CommitRequest,
    CommitScope, HookPolicy,
};

pub use crate::porcelain::CommitError;

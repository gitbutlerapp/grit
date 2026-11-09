//! Working-tree status for one repository ([`Status`] from [`Repository::status`](crate::repo::Repository::status)).
//!
//! ```rust
//! use grit_lib::progress::{CancelToken, NullProgress};
//! use grit_lib::repo::{InitOptions, Repository};
//! use grit_lib::status::StatusOptions;
//!
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let model = repo.status().run(
//!     &StatusOptions::default(),
//!     &mut NullProgress,
//!     &CancelToken::never(),
//! )?;
//! assert!(model.staged.is_empty() && model.unstaged.is_empty());
//! # Ok(())
//! # }
//! ```

mod handle;

pub use handle::{Status, UntrackedCachePolicy, UntrackedOptions};

pub use crate::porcelain::status::{
    format_porcelain_v2, short_status_lines, ConflictGroup, IgnoredMode, InterruptedRewrite,
    RenameDetection, StatusAction, StatusGuidance, StatusModel, StatusOptions, StatusSummary,
    StatusSummaryOptions, UntrackedCollectMeta, UntrackedMode,
};

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

mod case_collision;
mod handle;
mod summary;

pub(crate) use case_collision::worktree_case_collision_groups;
pub use case_collision::{
    filter_guidance_for_case_collisions, worktree_case_collision_paths, CaseCollisionGroup,
};
pub use handle::{Status, UntrackedCachePolicy, UntrackedOptions};
pub use summary::{StatusSummary, StatusSummaryOptions};

pub use crate::porcelain::status::{
    format_porcelain_v2, short_status_lines, ConflictGroup, IgnoredMode, InterruptedRewrite,
    RenameDetection, StatusAction, StatusGuidance, StatusModel, StatusOptions,
    UntrackedCollectMeta, UntrackedMode,
};

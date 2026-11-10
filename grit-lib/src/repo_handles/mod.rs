//! Typed [`Repository`](crate::repo::Repository) handles for remotes, worktrees, stash, notes, reflogs, and bundles.
//!
//! Each handle borrows the repository and exposes domain operations with typed options
//! instead of scattering free functions across the crate root.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/repository)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.remotes();
//!
//! # Ok(())
//! # }
//! ```

mod branches;
mod bundle;
mod checkout;
mod hooks;
mod notes;
mod reflog;
mod remote;
mod remote_rename;
mod stash;
mod tags;
mod workspace_snapshot;
mod worktree;

pub use crate::reflog::MessageMatcher;
#[cfg(feature = "unstable-hosting")]
pub use crate::snapshot_checkout::handle::RepositorySnapshotCheckouts;
pub use branches::Branches;
pub use bundle::RepositoryBundles;
pub use checkout::Checkout;
pub use hooks::{HookInfo, HookSource, RepositoryHooks};
pub use notes::{
    Note, NoteAddOutcome, NoteAddPlan, NoteListing, NoteRemoveManyPlan, NoteRemovePlan,
    NotesWriteOptions, RepositoryNotes,
};
pub use reflog::{
    ExpireOptions, IndexedReflogEntry, RefExpireResult, ReflogCreate, ReflogExpireOutcome,
    ReflogPage, ReflogQuery, RepositoryReflogs,
};
pub use remote::has_remote_section;
pub use remote::{
    ConfiguredRemote, RemoteRemoval, RemoteUrlKind, RemoveOptions, RepositoryRemotes, SetHead,
    SetHeadOutcome, UrlChange,
};
pub use remote_rename::RemoteRename;
pub use stash::RepositoryStash;
pub use tags::{TagListOptions, Tags};
pub use workspace_snapshot::RepositoryWorkspaceSnapshot;
pub use worktree::{LockWorktreeOptions, RepositoryWorktrees};

/// Short alias for [`RepositoryBundles`].
pub type Bundles<'repo> = RepositoryBundles<'repo>;
/// Short alias for [`RepositorySnapshotCheckouts`].
#[cfg(feature = "unstable-hosting")]
pub type SnapshotCheckouts<'repo> = RepositorySnapshotCheckouts<'repo>;
/// Short alias for [`RepositoryRemotes`].
pub type Remotes<'repo> = RepositoryRemotes<'repo>;
/// Short alias for [`RepositoryWorktrees`].
pub type Worktrees<'repo> = RepositoryWorktrees<'repo>;

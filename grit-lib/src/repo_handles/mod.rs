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
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.remotes();
//!
//! # Ok(())
//! # }
//! ```

mod bundle;
mod hooks;
mod notes;
mod reflog;
mod remote;
mod remote_rename;
mod stash;
mod workspace_snapshot;
mod worktree;

pub use crate::reflog::MessageMatcher;
pub use crate::remote::{
    InspectOptions, RemoteBranchState, RemoteBranchStateKind, RemotePullBranch, RemoteSummary,
};
pub use bundle::RepositoryBundles;
pub use hooks::{HookInfo, HookSource, RepositoryHooks};
pub use notes::{
    Note, NoteAddOutcome, NoteAddPlan, NoteListing, NoteRemoveManyPlan, NoteRemovePlan,
    NotesWriteOptions, RepositoryNotes,
};
pub use reflog::{
    ExpireOptions, IndexedReflogEntry, RefExpireResult, ReflogExpireOutcome, ReflogPage,
    ReflogQuery, RepositoryReflogs,
};
pub use remote::has_remote_section;
pub use remote::{
    configured_remote_names, configured_remote_names_in_config_order, configured_remotes,
    remote_skip_fetch_all, ConfiguredRemote, RemoteRemoval, RemoteUrlKind, RemoveOptions,
    RepositoryRemotes, SetHead, SetHeadOutcome, UrlChange,
};
pub use remote_rename::RemoteRename;
pub use stash::RepositoryStash;
pub use workspace_snapshot::RepositoryWorkspaceSnapshot;
pub use worktree::{LockWorktreeOptions, RepositoryWorktrees};

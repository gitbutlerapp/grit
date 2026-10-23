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
mod stash;
mod worktree;

pub use bundle::RepositoryBundles;
pub use hooks::{HookInfo, HookSource, RepositoryHooks};
pub use notes::{Note, NoteAddOutcome, NoteListing, NotesWriteOptions, RepositoryNotes};
pub use reflog::{ExpireOptions, IndexedReflogEntry, ReflogExpireOutcome, RepositoryReflogs};
pub(crate) use remote::has_remote_section;
pub use remote::{
    configured_remote_names, configured_remote_names_in_config_order, configured_remotes,
    remote_skip_fetch_all, ConfiguredRemote, RemoteRemoval, RemoveOptions, RepositoryRemotes,
};
pub use stash::{RepositoryStash, StashApplyOptions, StashPushOptions};
pub use worktree::{LockWorktreeOptions, RepositoryWorktrees};

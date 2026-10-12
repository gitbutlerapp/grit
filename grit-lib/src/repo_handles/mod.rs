//! Typed [`Repository`](crate::repo::Repository) handles for remotes, worktrees, stash, notes, and bundles.
//!
//! Each handle borrows the repository and exposes domain operations with typed options
//! instead of scattering free functions across the crate root.

mod bundle;
mod notes;
mod remote;
mod stash;
mod worktree;

pub use bundle::RepositoryBundles;
pub use notes::{NotesWriteOptions, RepositoryNotes};
pub use remote::RepositoryRemotes;
pub use stash::{RepositoryStash, StashApplyOptions, StashPushOptions};
pub use worktree::{LockWorktreeOptions, RepositoryWorktrees};

//! Bundle files and bundle-over-remote transport.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/bundle)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.bundles();
//!
//! # Ok(())
//! # }
//! ```
mod core;

pub use core::*;

pub mod bundle_remote {
    //! Revision options for writing bundles from `grit bundle create`-style arguments.
    pub use crate::bundle_remote::BundleCreateOptions;
}

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
    //! bundle_remote API (re-exported at this path).
    pub use crate::bundle_remote::{
        fetch_from_bundle_with_progress, write_bundle_from_rev_specs, write_bundle_with_options,
        BundleCreateOptions,
    };
}

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

pub use core::BundleCreatePlan;
pub use core::BundlePrerequisite;
pub use core::BundleRefEntry;
pub use core::BundleRefUpdatePlan;
pub use core::BundleUnbundlePlan;
pub use core::BundleVersion;
pub use core::UnbundledRef;
pub(crate) use core::{
    plan_bundle_create, plan_bundle_unbundle, write_bundle, write_bundle_header,
};
pub use core::{
    read_header, Bundle, BundleError, BundleHeader, BundleSpec, BundleVerifyReport, FilterSpec,
    UnbundleOutcome,
};

pub mod bundle_remote {
    //! Revision options for writing bundles from `grit bundle create`-style arguments.
    pub use crate::bundle_remote::BundleCreateOptions;
}

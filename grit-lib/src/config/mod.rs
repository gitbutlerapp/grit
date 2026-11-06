//! Configuration cascade, identity, and repository bootstrap helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/config)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _cfg = _repo.config().snapshot()?;
//!
//! # Ok(())
//! # }
//! ```
mod core;
pub(crate) mod handle;
mod location;
mod typed;
mod write;

pub use core::*;
pub use handle::Config;
#[allow(deprecated)]
pub use location::{
    add_config_value, global_config_write_path, remove_config_section, rename_config_section,
    set_config_value, unset_all_config_values, unset_config_value, ConfigLocation,
    ConfigWritePreview,
};
pub use typed::{
    canonicalize_config_write_value, parse_typed_value, ConfigValueType, TypedConfigValue,
};
pub use write::{ConfigEditor, ConfigWriteOptions};

pub mod dotfile {
    //! dotfile API (re-exported at this path).
    pub use crate::dotfile::*;
}
pub mod environment {
    //! environment API (re-exported at this path).
    pub use crate::environment::*;
}
pub mod gitmodules {
    //! gitmodules API (re-exported at this path).
    pub use crate::gitmodules::*;
    /// Submodule `.git` file resolution.
    pub mod gitdir {
        pub use crate::submodule_gitdir::*;
    }
}
pub mod ident_resolve {
    //! ident_resolve API (re-exported at this path).
    pub use crate::ident_resolve::*;
}
pub mod init_filesystem {
    //! init_filesystem API (re-exported at this path).
    pub use crate::init_filesystem::*;
}
pub mod precompose_config {
    //! precompose_config API (re-exported at this path).
    pub use crate::precompose_config::*;
}
pub mod git_date {
    //! git_date API (re-exported at this path).
    pub use crate::git_date::*;
}
pub mod git_path {
    //! git_path API (re-exported at this path).
    pub use crate::git_path::*;
}
pub mod mailmap {
    //! mailmap API (re-exported at this path).
    pub use crate::mailmap::*;
}

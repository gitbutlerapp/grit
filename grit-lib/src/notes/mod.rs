//! Git notes refs and note commit helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/notes)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.notes();
//!
//! # Ok(())
//! # }
//! ```
mod core;

pub use core::*;

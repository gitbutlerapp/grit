//! Typed ref storage backend selection (`files` vs `reftable`).
//!
//! Discovery reads **repository-local** config only (never global or system config),
//! matching legacy reftable detection (see `repository_config_snapshot_tests`).

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::repo::{read_repository_format_from_git_dir, validate_repository_format_parsed};

/// On-disk ref storage backend named by `extensions.refStorage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefStorageFormat {
    /// Traditional loose refs under `refs/` plus `packed-refs`.
    #[default]
    Files,
    /// Git reftable backend (`extensions.refStorage = reftable`).
    Reftable,
}

impl fmt::Display for RefStorageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Files => "files",
            Self::Reftable => "reftable",
        })
    }
}

impl FromStr for RefStorageFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse_config_value(s)
    }
}

impl RefStorageFormat {
    /// Parse an `extensions.refStorage` config value (optional `name:payload` suffix).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRefStorageFormat`] when the name is not `files` or `reftable`.
    pub fn parse_config_value(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        let name = lower
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .unwrap_or(lower.as_str());
        match name {
            "files" => Ok(Self::Files),
            "reftable" => Ok(Self::Reftable),
            _ => Err(Error::InvalidRefStorageFormat {
                value: trimmed.to_owned(),
            }),
        }
    }

    /// Detect ref storage from repository-local `config` (linked worktree common dir when needed).
    ///
    /// When `extensions.refStorage` is absent, returns [`RefStorageFormat::Files`].
    ///
    /// # Errors
    ///
    /// Propagates I/O and repository format errors when local config is invalid.
    pub fn detect(git_dir: &Path) -> Result<Self> {
        let parsed = read_repository_format_from_git_dir(git_dir)?;
        validate_repository_format_parsed(&parsed)?;
        match parsed.ref_storage.as_deref() {
            None => Ok(Self::Files),
            Some(raw) => Self::parse_config_value(raw),
        }
    }

    /// Whether this format uses the reftable backend.
    #[must_use]
    pub const fn is_reftable(self) -> bool {
        matches!(self, Self::Reftable)
    }
}

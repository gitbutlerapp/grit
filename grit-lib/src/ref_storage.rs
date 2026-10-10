//! Typed ref storage backend selection (`files` vs `reftable`).
//!
//! Discovery reads **repository-local** config only (never global or system config),
//! matching [`crate::reftable::is_reftable_repo`] (see `repository_config_snapshot_tests`).

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::repo::read_repository_format_from_git_dir;

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
    /// Reads `core.repositoryformatversion` as part of format parsing but does not reject
    /// v0 repositories that declare reftable here — [`crate::repo::validate_repo_format`] handles that.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when local config cannot be read, or
    /// [`Error::InvalidRefStorageFormat`] for unknown `extensions.refStorage` values.
    pub fn detect(git_dir: &Path) -> Result<Self> {
        let parsed = read_repository_format_from_git_dir(git_dir)?;
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

#[cfg(test)]
mod ref_storage_format {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::repo::init_repository;

    #[test]
    fn parses_files_and_reftable() {
        assert_eq!(
            RefStorageFormat::from_str("files").unwrap(),
            RefStorageFormat::Files
        );
        assert_eq!(
            RefStorageFormat::from_str("REFTABLE").unwrap(),
            RefStorageFormat::Reftable
        );
        assert_eq!(
            RefStorageFormat::parse_config_value("reftable").unwrap(),
            RefStorageFormat::Reftable
        );
    }

    #[test]
    fn rejects_unknown() {
        let err = RefStorageFormat::from_str("not-a-backend").unwrap_err();
        assert!(matches!(err, Error::InvalidRefStorageFormat { .. }));
    }

    #[test]
    fn payload_suffix_accepted() {
        assert_eq!(
            RefStorageFormat::parse_config_value("files:v1").unwrap(),
            RefStorageFormat::Files
        );
        assert_eq!(
            RefStorageFormat::parse_config_value("reftable:experimental").unwrap(),
            RefStorageFormat::Reftable
        );
    }

    #[test]
    fn ignores_global_config() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.gitconfig");
        fs::write(&global, "[extensions]\n\trefstorage = reftable\n").unwrap();
        let root = tmp.path().join("repo");
        init_repository(&root, false, "main", None, RefStorageFormat::Files).unwrap();
        let git_dir = root.join(".git");

        let prev = std::env::var("GIT_CONFIG_GLOBAL").ok();
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");

        assert_eq!(
            RefStorageFormat::detect(&git_dir).unwrap(),
            RefStorageFormat::Files
        );

        if let Some(v) = prev {
            std::env::set_var("GIT_CONFIG_GLOBAL", v);
        } else {
            std::env::remove_var("GIT_CONFIG_GLOBAL");
        }
    }

    #[test]
    fn detect_reads_local_reftable_config() {
        let tmp = TempDir::new().unwrap();
        init_repository(tmp.path(), false, "main", None, RefStorageFormat::Reftable).unwrap();
        let git_dir = tmp.path().join(".git");
        assert_eq!(
            RefStorageFormat::detect(&git_dir).unwrap(),
            RefStorageFormat::Reftable
        );
    }
}

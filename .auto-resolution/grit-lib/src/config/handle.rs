//! Repository-scoped configuration reads and local writes via [`ConfigHandle`].
use std::fs;
use std::sync::Arc;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::Result;
use crate::repo::Repository;

/// Read merged config and update the repository-local config file.
#[derive(Debug, Copy, Clone)]
pub struct ConfigHandle<'repo> {
    repo: &'repo Repository,
}

impl<'repo> ConfigHandle<'repo> {
    /// Bind config operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Look up a merged config value (system / global / local / worktree cascade).
    ///
    /// # Errors
    ///
    /// Propagates config load failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = std::env::temp_dir().join("grit-config-get-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let _ = repo.config_handle().get("core.repositoryformatversion");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.snapshot()?.get(key))
    }

    /// Set a key in the repository-local config file and reload the in-memory snapshot.
    ///
    /// For linked worktrees this writes `commondir/config` when the per-worktree git directory
    /// has no `config` file, matching Git's `--local` target.
    ///
    /// # Errors
    ///
    /// Propagates config parse, validation, and I/O errors.
    #[inline]
    pub fn set_local(&self, key: &str, value: &str) -> Result<()> {
        let path = crate::repo::local_config_write_path(self.repo.git_dir());
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        file.set(key, value)?;
        file.write()?;
        self.repo.reload_config()
    }

    /// Remove `key` from the repository-local config file and reload the snapshot.
    ///
    /// # Errors
    ///
    /// Propagates config parse, validation, and I/O errors.
    pub fn unset_local(&self, key: &str) -> Result<()> {
        let path = crate::repo::local_config_write_path(self.repo.git_dir());
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        file.unset(key)?;
        file.write()?;
        self.repo.reload_config()
    }

    /// Remove an entire section from the repository-local config file.
    ///
    /// Section names use dotted form (for example `branch.topic` or `branch.feature/x`).
    ///
    /// # Errors
    ///
    /// Propagates config parse, validation, and I/O errors.
    pub fn remove_local_section(&self, section: &str) -> Result<()> {
        let path = crate::repo::local_config_write_path(self.repo.git_dir());
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        file.remove_section(section)?;
        file.write()?;
        self.repo.reload_config()
    }

    /// Shared merged config snapshot (same as [`Repository::config`]).
    #[inline]
    pub fn snapshot(&self) -> Result<Arc<ConfigSet>> {
        self.repo.config()
    }
}

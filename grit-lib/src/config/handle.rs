//! Repository-scoped configuration reads and local writes via [`Config`].
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::Result;
use crate::repo::Repository;

/// Read merged config and update the repository-local config file.
#[derive(Debug, Copy, Clone)]
pub struct Config<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Config<'repo> {
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

    /// Shared merged config snapshot for this repository.
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
    /// let (repo, _) = Repository::init(&dir, &InitOptions::default())?;
    /// let _ = repo.config().get("core.repositoryformatversion")?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn snapshot(&self) -> Result<Arc<ConfigSet>> {
        self.repo.ensure_config_arc()
    }

    /// Look up a merged config value (system / global / local / worktree cascade).
    ///
    /// # Errors
    ///
    /// Propagates config load failures and [`crate::error::ConfigError::InvalidKey`] for malformed keys.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.snapshot()?.get(key)?)
    }

    /// All values for `key` in load order.
    pub fn get_all(&self, key: &str) -> Result<Vec<String>> {
        Ok(self.snapshot()?.get_all(key)?)
    }

    /// Parse a boolean config value.
    pub fn get_bool(&self, key: &str) -> Result<Option<bool>> {
        Ok(self.snapshot()?.get_bool(key)?)
    }

    /// Parse an integer config value (Git `k`/`m`/`g` suffixes supported).
    pub fn get_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.snapshot()?.get_i64(key)?)
    }

    /// Expand a path config value using this repository's environment.
    pub fn get_path(&self, key: &str) -> Result<Option<PathBuf>> {
        Ok(self.snapshot()?.get_path(self.repo.environment(), key)?)
    }

    /// Parse a Git color value (`#rgb`, `normal`, `bold`, etc.).
    pub fn get_color(&self, key: &str) -> Result<Option<String>> {
        Ok(self.snapshot()?.get_color(key)?)
    }

    /// Entries whose canonical key starts with `prefix`.
    pub fn entries_with_prefix(&self, prefix: &str) -> Result<Vec<crate::config::ConfigEntry>> {
        self.snapshot()?.entries_with_prefix(prefix)
    }

    /// Set a key in the repository-local config file and reload the in-memory snapshot.
    #[deprecated(note = "use ConfigEditor and explicit reload where needed")]
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
    #[deprecated(note = "use ConfigEditor and explicit reload where needed")]
    pub fn unset_local(&self, key: &str) -> Result<()> {
        let path = crate::repo::local_config_write_path(self.repo.git_dir());
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        file.unset(key)?;
        file.write()?;
        self.repo.reload_config()
    }

    /// Remove an entire section from the repository-local config file.
    #[deprecated(note = "use ConfigEditor and explicit reload where needed")]
    pub fn remove_local_section(&self, section: &str) -> Result<()> {
        let path = crate::repo::local_config_write_path(self.repo.git_dir());
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        file.remove_section(section)?;
        file.write()?;
        self.repo.reload_config()
    }
}

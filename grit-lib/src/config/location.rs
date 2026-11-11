//! Config file locations (`--local`, `--global`, `-f`) and scoped load/write helpers.

use std::path::{Path, PathBuf};

use crate::environment::Environment;
use crate::error::{ConfigError, Error, Result};

use super::{
    global_config_paths_pub, resolve_config_override_path, system_config_path, ConfigFile,
    ConfigScope, ConfigSet, IncludeContext, IncludeOptions,
};

/// Which config file(s) to read or write.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigLocation {
    /// Repository `.git/config` (or `commondir/config` for linked worktrees).
    Local,
    /// Per-user global config (XDG + `~/.gitconfig`, or `GIT_CONFIG_GLOBAL`).
    Global,
    /// System-wide `/etc/gitconfig` (or platform equivalent).
    System,
    /// Per-worktree `config.worktree` (when enabled).
    Worktree,
    /// A single file from `-f` / `--file`.
    File(PathBuf),
}

impl ConfigLocation {
    /// Scope stored in parsed entries from this location's on-disk file.
    #[must_use]
    pub fn file_scope(&self) -> ConfigScope {
        match self {
            Self::Local => ConfigScope::Local,
            Self::File(_) => ConfigScope::Local,
            Self::Global => ConfigScope::Global,
            Self::System => ConfigScope::System,
            Self::Worktree => ConfigScope::Worktree,
        }
    }

    /// Path to create or update when writing a value at this location.
    ///
    /// Global writes follow Git's writer preference: an existing `~/.gitconfig`, then an
    /// existing XDG `git/config`, otherwise the conventional `~/.gitconfig`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the location needs a repository but none was given.
    pub fn write_path(&self, env: &Environment, git_dir: Option<&Path>) -> Result<PathBuf> {
        match self {
            Self::Global => global_config_write_path(env),
            Self::Local => {
                let gd = git_dir.ok_or_else(|| Error::Config(ConfigError::RepositoryRequired))?;
                Ok(crate::repo::local_config_write_path(gd))
            }
            Self::File(path) => {
                let s = path.to_string_lossy();
                Ok(resolve_config_override_path(env, s.as_ref()))
            }
            Self::System => Ok(system_config_path(env)),
            Self::Worktree => {
                let gd = git_dir.ok_or_else(|| Error::Config(ConfigError::RepositoryRequired))?;
                Ok(gd.join("config.worktree"))
            }
        }
    }

    /// Load configuration from this location only (no full cascade).
    ///
    /// `[include]` / `[includeIf]` directives are expanded. Command-line overrides are not
    /// included except for the full-repository [`ConfigSet::load`] path used by the CLI default
    /// read mode.
    pub fn load_single(&self, env: &Environment, git_dir: Option<&Path>) -> Result<ConfigSet> {
        let mut set = ConfigSet::new();
        let includes = IncludeOptions::default().with_context(IncludeContext {
            git_dir: git_dir.map(Path::to_path_buf),
            command_line_relative_include_is_error: false,
            cwd: env.cwd.clone(),
            pwd: env.pwd.clone(),
            env: std::sync::Arc::new(env.clone()),
        });

        match self {
            Self::Global => {
                for path in global_config_paths_pub(env) {
                    if let Some(f) = ConfigFile::from_path(&path, ConfigScope::Global)? {
                        set.merge_file(&f, &includes)?;
                    }
                }
            }
            Self::Local => {
                let gd = git_dir.ok_or_else(|| Error::Config(ConfigError::RepositoryRequired))?;
                let local_path = crate::repo::local_config_write_path(gd);
                if let Some(f) = ConfigFile::from_path(&local_path, ConfigScope::Local)? {
                    set.merge_file(&f, &includes)?;
                }
            }
            Self::System => {
                let system_path = system_config_path(env);
                if let Some(f) = ConfigFile::from_path(&system_path, ConfigScope::System)? {
                    set.merge_file(&f, &includes)?;
                }
            }
            Self::Worktree => {
                let gd = git_dir.ok_or_else(|| Error::Config(ConfigError::RepositoryRequired))?;
                let common = crate::repo::common_git_dir_for_config(gd);
                if !crate::repo::worktree_config_enabled(&common) {
                    return Ok(set);
                }
                let wt_path = gd.join("config.worktree");
                if let Some(f) = ConfigFile::from_path(&wt_path, ConfigScope::Worktree)? {
                    set.merge_file(&f, &includes)?;
                }
            }
            Self::File(path) => {
                let s = path.to_string_lossy();
                let path = resolve_config_override_path(env, s.as_ref());
                if let Some(f) = ConfigFile::from_path(&path, ConfigScope::Command)? {
                    set.merge_file(&f, &includes)?;
                }
            }
        }

        Ok(set)
    }
}

/// Global config file path for writes, matching Git's preference for an existing file.
///
/// When `GIT_CONFIG_GLOBAL` is set, that path is always used. Otherwise prefer an existing
/// `~/.gitconfig`, then an existing XDG `git/config`, then create `~/.gitconfig`.
pub fn global_config_write_path(env: &Environment) -> Result<PathBuf> {
    let paths = global_config_paths_pub(env);
    let Some(dotgitconfig) = paths.last() else {
        return Err(Error::Config(ConfigError::NoGlobalConfigPath));
    };
    if paths.len() == 1 || dotgitconfig.exists() {
        return Ok(dotgitconfig.clone());
    }
    let existing_xdg = paths.first().filter(|xdg| xdg.exists());
    Ok(existing_xdg.unwrap_or(dotgitconfig).clone())
}

/// Human-readable summary of a config write that `--dry-run` would perform.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigWritePreview {
    /// Config file that would be updated.
    pub path: PathBuf,
    /// Short description of the change.
    pub summary: String,
}

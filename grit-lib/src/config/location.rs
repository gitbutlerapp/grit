//! Config file locations (`--local`, `--global`, `-f`) and scoped load/write helpers.

use std::fs;
use std::path::{Path, PathBuf};

use crate::environment::Environment;
use crate::error::{ConfigError, Error, Result};

use super::{
    global_config_paths_pub, resolve_config_override_path, system_config_path, ConfigFile,
    ConfigScope, ConfigSet, IncludeContext,
};

/// Which config file(s) to read or write.
#[derive(Debug, Clone, PartialEq, Eq)]
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
                let gd = git_dir.ok_or_else(|| {
                    Error::Config(ConfigError::Other("not in a git repository".into()))
                })?;
                Ok(crate::repo::local_config_write_path(gd))
            }
            Self::File(path) => {
                let s = path.to_string_lossy();
                Ok(resolve_config_override_path(env, s.as_ref()))
            }
            Self::System => Ok(system_config_path(env)),
            Self::Worktree => {
                let gd = git_dir.ok_or_else(|| {
                    Error::Config(ConfigError::Other("not in a git repository".into()))
                })?;
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
        let ctx = IncludeContext {
            git_dir: git_dir.map(Path::to_path_buf),
            command_line_relative_include_is_error: false,
            cwd: env.cwd.clone(),
            pwd: env.pwd.clone(),
            env: std::sync::Arc::new(env.clone()),
        };

        match self {
            Self::Global => {
                for path in global_config_paths_pub(env) {
                    if let Some(f) = ConfigFile::from_path(&path, ConfigScope::Global)? {
                        set.merge_file_with_includes(&f, true, &ctx)?;
                    }
                }
            }
            Self::Local => {
                let gd = git_dir.ok_or_else(|| {
                    Error::Config(ConfigError::Other("not in a git repository".into()))
                })?;
                let local_path = crate::repo::local_config_write_path(gd);
                if let Some(f) = ConfigFile::from_path(&local_path, ConfigScope::Local)? {
                    set.merge_file_with_includes(&f, true, &ctx)?;
                }
            }
            Self::System => {
                let system_path = system_config_path(env);
                if let Some(f) = ConfigFile::from_path(&system_path, ConfigScope::System)? {
                    set.merge_file_with_includes(&f, true, &ctx)?;
                }
            }
            Self::Worktree => {
                let gd = git_dir.ok_or_else(|| {
                    Error::Config(ConfigError::Other("not in a git repository".into()))
                })?;
                let wt_path = gd.join("config.worktree");
                if let Some(f) = ConfigFile::from_path(&wt_path, ConfigScope::Worktree)? {
                    set.merge_file_with_includes(&f, true, &ctx)?;
                }
            }
            Self::File(path) => {
                let s = path.to_string_lossy();
                let path = resolve_config_override_path(env, s.as_ref());
                if let Some(f) = ConfigFile::from_path(&path, ConfigScope::Command)? {
                    set.merge_file_with_includes(&f, true, &ctx)?;
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
    if paths.len() == 1 {
        return paths.into_iter().next().ok_or_else(|| {
            Error::Config(ConfigError::Other(
                "could not determine global config path".into(),
            ))
        });
    }
    let dotgitconfig = paths.last().cloned();
    if dotgitconfig.as_ref().is_some_and(|p| p.exists()) {
        return dotgitconfig.ok_or_else(|| {
            Error::Config(ConfigError::Other(
                "could not determine global config path".into(),
            ))
        });
    }
    if let Some(xdg) = paths.first() {
        if xdg.exists() {
            return Ok(xdg.clone());
        }
    }
    dotgitconfig.ok_or_else(|| {
        Error::Config(ConfigError::Other(
            "could not determine global config path".into(),
        ))
    })
}

/// Set `key` to `value` in the config file for `location`.
pub fn set_config_value(
    location: ConfigLocation,
    env: &Environment,
    git_dir: Option<&Path>,
    key: &str,
    value: &str,
) -> Result<()> {
    let path = location.write_path(env, git_dir)?;
    let scope = location.file_scope();
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, scope)?;
    cfg.set(key, value)?;
    cfg.write()?;
    Ok(())
}

/// Remove `key` from the config file for `location`.
///
/// # Errors
///
/// Returns [`ConfigError::KeyNotSet`] when the key is absent in that file.
pub fn unset_config_value(
    location: ConfigLocation,
    env: &Environment,
    git_dir: Option<&Path>,
    key: &str,
) -> Result<()> {
    let path = location.write_path(env, git_dir)?;
    let scope = location.file_scope();
    let Ok(content) = fs::read_to_string(&path) else {
        return Err(Error::Config(ConfigError::KeyNotSet {
            key: key.to_owned(),
        }));
    };
    let mut cfg = ConfigFile::parse(&path, &content, scope)?;
    let removed = cfg.unset(key)?;
    if removed == 0 {
        return Err(Error::Config(ConfigError::KeyNotSet {
            key: key.to_owned(),
        }));
    }
    cfg.write()?;
    Ok(())
}

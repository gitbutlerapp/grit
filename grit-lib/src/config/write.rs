//! Scoped config file writes for [`Config`](super::handle::Config) and [`ConfigEditor`].
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use super::{
    canonicalize_config_write_value, global_config_write_path, resolve_config_override_path,
    system_config_path, ConfigFile, ConfigScope, ConfigValueType, ConfigWritePreview,
};
use crate::environment::Environment;
use crate::error::{ConfigError, Error, Result};

/// Options for a config file write (dry-run, typed canonicalization, unset patterns).
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ConfigWriteOptions {
    dry_run: bool,
    value_type: Option<ConfigValueType>,
    /// When true, [`ConfigEditor::set`] and [`Config::set`] call [`ConfigFile::replace_all`].
    replace_all: bool,
    /// Values to replace must match this pattern when using replace-all (Git value pattern).
    replace_value_pattern: Option<String>,
    unset_value_pattern: Option<String>,
}

impl ConfigWriteOptions {
    /// Default options: persist changes and accept values as given.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// When `true`, return a [`ConfigWritePreview`] instead of writing disk.
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Canonicalize `value` with Git `--type` rules before writing.
    #[must_use]
    pub fn value_type(mut self, ty: ConfigValueType) -> Self {
        self.value_type = Some(ty);
        self
    }

    /// Replace every occurrence of `key` with `value` (Git `replace-all` semantics).
    ///
    /// When `false` (the default), a single-valued `set` overwrites one slot; see [`ConfigFile::set`].
    #[must_use]
    pub fn replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = replace_all;
        self
    }

    /// When using [`Self::replace_all`], only replace values matching this Git config value pattern.
    #[must_use]
    pub fn replace_value_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.replace_value_pattern = Some(pattern.into());
        self
    }

    /// When unsetting, remove only values matching this pattern (`unset-all` / `--value`).
    #[must_use]
    pub fn unset_value_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.unset_value_pattern = Some(pattern.into());
        self
    }

    /// Whether this write should only preview the change.
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }
}

/// Edit a config file outside an open [`Repository`](crate::repo::Repository) (global, system, or `-f` path).
#[derive(Debug, Clone)]
pub struct ConfigEditor {
    path: PathBuf,
    file_scope: ConfigScope,
    env: Environment,
}

impl ConfigEditor {
    /// Open the per-user global config file for writes (respects `GIT_CONFIG_GLOBAL` / XDG).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoGlobalConfigPath`] when no global path can be resolved.
    pub fn global(env: &Environment) -> Result<Self> {
        Ok(Self {
            path: global_config_write_path(env)?,
            file_scope: ConfigScope::Global,
            env: env.clone(),
        })
    }

    /// Open the system config file for writes.
    #[must_use]
    pub fn system(env: &Environment) -> Self {
        Self {
            path: system_config_path(env),
            file_scope: ConfigScope::System,
            env: env.clone(),
        }
    }

    /// Open an explicit config file (for example `grit config -f path`).
    ///
    /// `file_scope` is the scope stored in parsed entries (typically [`ConfigScope::Local`]).
    #[must_use]
    pub fn open(env: &Environment, path: PathBuf, file_scope: ConfigScope) -> Self {
        let path = resolve_config_override_path(env, path.to_string_lossy().as_ref());
        Self {
            path,
            file_scope,
            env: env.clone(),
        }
    }

    /// Path that would be read or written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Set `key` to `value`, replacing an existing single-valued key.
    pub fn set(
        &self,
        key: &str,
        value: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let value = canonicalize_value(key, value, options, &self.env)?;
        write_file(&self.path, self.file_scope, None, options, |cfg| {
            if options.replace_all {
                cfg.replace_all(key, &value, options.replace_value_pattern.as_deref())?;
                return Ok(format!(
                    "replace-all {key}={value}{}",
                    options
                        .replace_value_pattern
                        .as_ref()
                        .map(|p| format!(" (pattern {p})"))
                        .unwrap_or_default()
                ));
            }
            cfg.set(key, &value)?;
            Ok(format!("set {key}={value}"))
        })
    }

    /// Append `value` for a multi-valued `key`.
    pub fn add_value(
        &self,
        key: &str,
        value: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let value = canonicalize_value(key, value, options, &self.env)?;
        write_file(&self.path, self.file_scope, None, options, |cfg| {
            cfg.add_value(key, &value)?;
            Ok(format!("add {key}={value}"))
        })
    }

    /// Remove the last value for `key`.
    pub fn unset(
        &self,
        key: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        write_file(&self.path, self.file_scope, None, options, |cfg| {
            if !self.path.exists() {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            let removed = cfg.unset_last(key)?;
            if removed == 0 {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            Ok(format!("unset {key}"))
        })
    }

    /// Remove every value for `key`, optionally filtered by [`ConfigWriteOptions::unset_value_pattern`].
    pub fn unset_all(
        &self,
        key: &str,
        options: &ConfigWriteOptions,
    ) -> Result<(usize, Option<ConfigWritePreview>)> {
        let pattern = options.unset_value_pattern.as_deref();
        let mut removed_count = 0usize;
        let preview = write_file(&self.path, self.file_scope, None, options, |cfg| {
            let removed = cfg.unset_matching(key, pattern, false)?;
            if removed == 0 {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            removed_count = removed;
            Ok(format!("unset-all {key} ({removed} values)"))
        })?;
        Ok((removed_count, preview))
    }

    /// Rename a section in the file.
    pub fn rename_section(
        &self,
        old_name: &str,
        new_name: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        write_file(&self.path, self.file_scope, None, options, |cfg| {
            if !cfg.rename_section(old_name, new_name)? {
                return Err(Error::Config(ConfigError::SectionNotFound {
                    section: old_name.to_owned(),
                }));
            }
            Ok(format!("rename-section {old_name} -> {new_name}"))
        })
    }

    /// Remove an entire section from the file.
    pub fn remove_section(
        &self,
        section: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        write_file(&self.path, self.file_scope, None, options, |cfg| {
            if !cfg.remove_section(section)? {
                return Err(Error::Config(ConfigError::SectionNotFound {
                    section: section.to_owned(),
                }));
            }
            Ok(format!("remove-section {section}"))
        })
    }
}

impl<'repo> super::Config<'repo> {
    /// Set `key` to `value` in the config file for `scope`.
    pub fn set(
        &self,
        scope: ConfigScope,
        key: &str,
        value: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let env = self.repository().environment();
        let value = canonicalize_value(key, value, options, env)?;
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            if options.replace_all {
                cfg.replace_all(key, &value, options.replace_value_pattern.as_deref())?;
                return Ok(format!(
                    "replace-all {key}={value}{}",
                    options
                        .replace_value_pattern
                        .as_ref()
                        .map(|p| format!(" (pattern {p})"))
                        .unwrap_or_default()
                ));
            }
            cfg.set(key, &value)?;
            Ok(format!("set {key}={value}"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok(preview)
    }

    /// Append `value` for a multi-valued `key` in the config file for `scope`.
    pub fn add_value(
        &self,
        scope: ConfigScope,
        key: &str,
        value: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let env = self.repository().environment();
        let value = canonicalize_value(key, value, options, env)?;
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            cfg.add_value(key, &value)?;
            Ok(format!("add {key}={value}"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok(preview)
    }

    /// Remove the last value for `key` in the config file for `scope`.
    pub fn unset(
        &self,
        scope: ConfigScope,
        key: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let env = self.repository().environment();
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            if !path.exists() {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            let removed = cfg.unset_last(key)?;
            if removed == 0 {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            Ok(format!("unset {key}"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok(preview)
    }

    /// Remove every value for `key` in the config file for `scope`.
    pub fn unset_all(
        &self,
        scope: ConfigScope,
        key: &str,
        options: &ConfigWriteOptions,
    ) -> Result<(usize, Option<ConfigWritePreview>)> {
        let env = self.repository().environment();
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let pattern = options.unset_value_pattern.as_deref();
        let mut removed_count = 0usize;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            let removed = cfg.unset_matching(key, pattern, false)?;
            if removed == 0 {
                return Err(Error::Config(ConfigError::KeyNotSet {
                    key: key.to_owned(),
                }));
            }
            removed_count = removed;
            Ok(format!("unset-all {key} ({removed} values)"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok((removed_count, preview))
    }

    /// Rename a section in the config file for `scope`.
    pub fn rename_section(
        &self,
        scope: ConfigScope,
        old_name: &str,
        new_name: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let env = self.repository().environment();
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            if !cfg.rename_section(old_name, new_name)? {
                return Err(Error::Config(ConfigError::SectionNotFound {
                    section: old_name.to_owned(),
                }));
            }
            Ok(format!("rename-section {old_name} -> {new_name}"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok(preview)
    }

    /// Remove an entire section from the config file for `scope`.
    pub fn remove_section(
        &self,
        scope: ConfigScope,
        section: &str,
        options: &ConfigWriteOptions,
    ) -> Result<Option<ConfigWritePreview>> {
        let env = self.repository().environment();
        let git_dir = self.repository().git_dir();
        let path = write_path_for_scope(scope, env, git_dir)?;
        let preview = write_file(&path, scope.file_scope(), Some(git_dir), options, |cfg| {
            if !cfg.remove_section(section)? {
                return Err(Error::Config(ConfigError::SectionNotFound {
                    section: section.to_owned(),
                }));
            }
            Ok(format!("remove-section {section}"))
        })?;
        if preview.is_none() {
            self.repository().reload_config()?;
        }
        Ok(preview)
    }
}

impl ConfigScope {
    /// Scope tag stored in [`ConfigFile`] entries for this write target.
    #[must_use]
    pub(crate) fn file_scope(self) -> ConfigScope {
        match self {
            Self::System | Self::Global | Self::Local | Self::Worktree => self,
            Self::Command => ConfigScope::Local,
        }
    }
}

fn canonicalize_value(
    key: &str,
    value: &str,
    options: &ConfigWriteOptions,
    env: &Environment,
) -> Result<String> {
    match options.value_type {
        Some(ty) => canonicalize_config_write_value(key, ty, value, env),
        None => Ok(value.to_owned()),
    }
}

fn write_path_for_scope(scope: ConfigScope, env: &Environment, git_dir: &Path) -> Result<PathBuf> {
    match scope {
        ConfigScope::Global => global_config_write_path(env),
        ConfigScope::Local => Ok(crate::repo::local_config_write_path(git_dir)),
        ConfigScope::System => Ok(system_config_path(env)),
        ConfigScope::Worktree => {
            ensure_worktree_write_allowed(git_dir)?;
            Ok(git_dir.join("config.worktree"))
        }
        ConfigScope::Command => Err(Error::Config(ConfigError::UnknownValue {
            key: "config scope".to_owned(),
            value: "command".to_owned(),
        })),
    }
}

fn ensure_worktree_write_allowed(git_dir: &Path) -> Result<()> {
    let common = crate::repo::common_git_dir_for_config(git_dir);
    if !crate::repo::worktree_config_enabled(&common) {
        return Err(Error::Config(ConfigError::WorktreeConfigNotEnabled));
    }
    Ok(())
}

fn open_config_file(path: &Path, scope: ConfigScope) -> Result<ConfigFile> {
    let content = match fs::read(path) {
        Ok(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => text.to_owned(),
            Err(_) => {
                return Err(Error::Io(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("config file {} is not valid UTF-8", path.display()),
                )));
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(Error::Io(err)),
    };
    ConfigFile::parse(path, &content, scope)
}

fn write_file(
    path: &Path,
    file_scope: ConfigScope,
    git_dir: Option<&Path>,
    options: &ConfigWriteOptions,
    mut apply: impl FnMut(&mut ConfigFile) -> Result<String>,
) -> Result<Option<ConfigWritePreview>> {
    if file_scope == ConfigScope::Worktree {
        let gd = git_dir.ok_or_else(|| Error::Config(ConfigError::RepositoryRequired))?;
        ensure_worktree_write_allowed(gd)?;
    }
    let mut cfg = open_config_file(path, file_scope)?;
    let summary = apply(&mut cfg)?;
    if options.dry_run {
        return Ok(Some(ConfigWritePreview {
            path: path.to_path_buf(),
            summary,
        }));
    }
    cfg.write()?;
    Ok(None)
}

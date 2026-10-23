//! `grit config` — read, set, and list configuration values.
//!
//! A deliberately small subset of `git config`: read a value, set one, list
//! everything, or unset a key. By default writes go to the repository's local
//! config (`.git/config`); `--global` and `-f` select other files.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use grit_lib::config::{
    canonical_key, set_config_value, unset_config_value, ConfigFile, ConfigLocation, ConfigScope,
    ConfigSet,
};
use grit_lib::error::ConfigError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

/// Result of `grit config`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConfigOutcome {
    Get {
        key: String,
        value: Option<String>,
        scope: Option<String>,
        file: Option<String>,
        line: Option<u64>,
    },
    List {
        entries: Vec<ConfigEntry>,
    },
    Set {
        key: String,
        value: String,
    },
    Unset {
        key: String,
    },
}

/// One entry in a `list` outcome. `value` is `null` for a bare/valueless key.
#[derive(Serialize, JsonSchema)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
    /// Which config layer this entry came from (`system`, `global`, `local`, …).
    pub scope: String,
    /// Source file, when the entry is file-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl HumanRender for ConfigOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ConfigOutcome::Get {
                value: Some(value), ..
            } => outln!(w, "{value}"),
            ConfigOutcome::Get { value: None, .. } => {}
            ConfigOutcome::List { entries } => {
                for entry in entries {
                    match &entry.value {
                        Some(value) => outln!(w, "{}={value}", entry.key),
                        None => outln!(w, "{}", entry.key),
                    }
                }
            }
            ConfigOutcome::Set { .. } | ConfigOutcome::Unset { .. } => {}
        }
        Ok(())
    }
}

impl MarkdownRender for ConfigOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ConfigOutcome::Get {
                key,
                value,
                scope,
                file,
                line,
            } => {
                outln!(w, "# Config `{key}`");
                outln!(w);
                match value {
                    Some(v) => outln!(w, "**Value:** `{v}`"),
                    None => outln!(w, "**Value:** _not set_"),
                }
                if let Some(scope) = scope {
                    outln!(w, "**Scope:** `{scope}`");
                }
                if let Some(file) = file {
                    outln!(w, "**File:** `{file}`");
                }
                if let Some(line) = line {
                    outln!(w, "**Line:** {line}");
                }
            }
            ConfigOutcome::List { entries } => markdown::print_config_entries(w, entries)?,
            ConfigOutcome::Set { key, value } => outln!(w, "Set `{key}` = {value}."),
            ConfigOutcome::Unset { key } => outln!(w, "Unset `{key}`."),
        }
        Ok(())
    }
}

/// Exit code for `get` when the key is missing (`1`); otherwise `0`.
#[must_use]
pub fn exit_code(outcome: &ConfigOutcome) -> i32 {
    match outcome {
        ConfigOutcome::Get { value: None, .. } => 1,
        _ => 0,
    }
}

/// Run `grit config`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    global: bool,
    local: bool,
    file: Option<PathBuf>,
    list: bool,
    unset: bool,
    key: Option<String>,
    value: Option<String>,
    mode: OutputMode,
) -> Result<ConfigOutcome> {
    if list {
        if key.is_some() || value.is_some() {
            bail!("--list does not take a key or value");
        }
        let env = context::environment();
        let read_location = resolve_location(global, local, file, false)?;
        return list_values(read_location, &env);
    }

    let Some(key) = key else {
        bail!("specify a config key (e.g. user.name), or pass --list to see everything");
    };

    if default_repo_hot_path(global, local, file.as_ref(), mode) {
        let env = context::environment();
        if unset {
            return hot_unset_in_repo(&key);
        }
        if let Some(value) = value {
            return hot_set_in_repo(&key, &value);
        }
        return hot_get_in_repo(&env, &key);
    }

    let write_location = resolve_location(global, local, file.clone(), true)?;
    let read_location = resolve_location(global, local, file, false)?;
    let env = context::environment();

    if unset {
        if value.is_some() {
            bail!("--unset does not take a value");
        }
        return unset_value(write_location, &env, &key);
    }

    match value {
        Some(value) => set_value(write_location, &env, &key, &value),
        None => get_value(read_location, &env, &key, mode),
    }
}

/// Default in-repo get/set/unset with human output (matches pre-scope CLI hot path).
fn default_repo_hot_path(
    global: bool,
    local: bool,
    file: Option<&PathBuf>,
    mode: OutputMode,
) -> bool {
    !global && !local && file.is_none() && matches!(mode, OutputMode::Human)
}

fn hot_get_in_repo(
    env: &grit_lib::config::environment::Environment,
    key: &str,
) -> Result<ConfigOutcome> {
    let config = read_merged_config(env)?;
    match config.get(key) {
        Some(value) => Ok(ConfigOutcome::Get {
            key: key.to_owned(),
            value: Some(value),
            scope: None,
            file: None,
            line: None,
        }),
        None => {
            canonical_key(key).context(format!("invalid config key {key}"))?;
            Ok(ConfigOutcome::Get {
                key: key.to_owned(),
                value: None,
                scope: None,
                file: None,
                line: None,
            })
        }
    }
}

fn hot_set_in_repo(key: &str, value: &str) -> Result<ConfigOutcome> {
    let repo = context::discover()?;
    let path = repo.git_dir().join("config");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Local)
        .with_context(|| format!("could not parse {}", path.display()))?;
    cfg.set(key, value)
        .with_context(|| format!("could not set {key}"))?;
    cfg.write()
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(ConfigOutcome::Set {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

fn hot_unset_in_repo(key: &str) -> Result<ConfigOutcome> {
    let repo = context::discover()?;
    let path = repo.git_dir().join("config");
    let Ok(content) = fs::read_to_string(&path) else {
        bail!("key '{key}' is not set");
    };
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Local)
        .with_context(|| format!("could not parse {}", path.display()))?;
    let removed = cfg
        .unset(key)
        .with_context(|| format!("could not unset {key}"))?;
    if removed == 0 {
        bail!("key '{key}' is not set");
    }
    cfg.write()
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(ConfigOutcome::Unset {
        key: key.to_owned(),
    })
}

fn read_merged_config(env: &grit_lib::config::environment::Environment) -> Result<ConfigSet> {
    if let Ok(repo) = context::discover() {
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")
    } else {
        ConfigSet::load(env, None, true).context("could not load config")
    }
}

/// Set `key` to `value` in the global config (used by `grit auth` to wire up the
/// Windows credential helper).
#[cfg(windows)]
pub fn set_global(key: &str, value: &str) -> Result<()> {
    set_value(
        Some(ConfigLocation::Global),
        &context::environment(),
        key,
        value,
    )
    .map(|_| ())
}

fn get_value(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    mode: OutputMode,
) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location, env)?;
    if matches!(mode, OutputMode::Human) {
        match config.get(key) {
            Some(value) => {
                return Ok(ConfigOutcome::Get {
                    key: key.to_owned(),
                    value: Some(value),
                    scope: None,
                    file: None,
                    line: None,
                });
            }
            None => {
                canonical_key(key).context(format!("invalid config key {key}"))?;
                return Ok(ConfigOutcome::Get {
                    key: key.to_owned(),
                    value: None,
                    scope: None,
                    file: None,
                    line: None,
                });
            }
        }
    }

    let origin = config
        .get_with_origin(key)
        .with_context(|| format!("invalid config key {key}"))?;
    Ok(match origin {
        Some(o) => ConfigOutcome::Get {
            key: key.to_owned(),
            value: o.value,
            scope: Some(o.scope.to_string()),
            file: o.file.as_ref().map(|p| p.display().to_string()),
            line: o.line.map(|l| l as u64),
        },
        None => ConfigOutcome::Get {
            key: key.to_owned(),
            value: None,
            scope: None,
            file: None,
            line: None,
        },
    })
}

fn list_values(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location, env)?;
    let entries = config
        .entries()
        .iter()
        .map(|entry| ConfigEntry {
            key: entry.key.clone(),
            value: entry.value.clone(),
            scope: entry.scope.to_string(),
            file: grit_lib::config::config_entry_origin_file(entry)
                .map(|p| p.display().to_string()),
        })
        .collect();
    Ok(ConfigOutcome::List { entries })
}

fn set_value(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    value: &str,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    set_config_value(location, env, git_dir.as_deref(), key, value)
        .with_context(|| format!("could not set {key}"))?;
    Ok(ConfigOutcome::Set {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

fn unset_value(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    match unset_config_value(location, env, git_dir.as_deref(), key) {
        Ok(()) => Ok(ConfigOutcome::Unset {
            key: key.to_owned(),
        }),
        Err(grit_lib::error::Error::Config(ConfigError::KeyNotSet { .. })) => {
            bail!("key '{key}' is not set");
        }
        Err(e) => Err(e.into()),
    }
}

/// `None` means load the full merged cascade (system + global + local + command).
fn load_for_read(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
) -> Result<ConfigSet> {
    if let Some(location) = read_location {
        let git_dir = optional_git_dir_for_read(&location)?;
        return location
            .load_single(env, git_dir.as_deref())
            .context("could not load config");
    }
    read_merged_config(env)
}

fn git_dir_for_location(location: &ConfigLocation) -> Result<Option<PathBuf>> {
    match location {
        ConfigLocation::Local | ConfigLocation::Worktree => {
            Ok(Some(context::discover()?.git_dir().to_path_buf()))
        }
        ConfigLocation::Global | ConfigLocation::System | ConfigLocation::File(_) => Ok(None),
    }
}

fn optional_git_dir_for_read(location: &ConfigLocation) -> Result<Option<PathBuf>> {
    match location {
        ConfigLocation::Local | ConfigLocation::Worktree => git_dir_for_location(location),
        ConfigLocation::Global | ConfigLocation::System | ConfigLocation::File(_) => {
            Ok(context::discover().ok().map(|r| r.git_dir().to_path_buf()))
        }
    }
}

fn resolve_location(
    global: bool,
    local: bool,
    file: Option<PathBuf>,
    for_write: bool,
) -> Result<Option<ConfigLocation>> {
    let flags = usize::from(global) + usize::from(local) + usize::from(file.is_some());
    if flags > 1 {
        bail!("--global, --local, and --file are mutually exclusive");
    }
    if let Some(path) = file {
        return Ok(Some(ConfigLocation::File(path)));
    }
    if global {
        return Ok(Some(ConfigLocation::Global));
    }
    if local {
        return Ok(Some(ConfigLocation::Local));
    }
    if for_write {
        Ok(Some(ConfigLocation::Local))
    } else {
        Ok(None)
    }
}

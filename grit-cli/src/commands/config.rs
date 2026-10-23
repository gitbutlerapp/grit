//! `grit config` — read, set, and list configuration values.
//!
//! A deliberately small subset of `git config`: read a value, set one, list
//! everything, or unset a key. By default writes go to the repository's local
//! config (`.git/config`); `--global` and `-f` select other files.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use grit_lib::config::{set_config_value, unset_config_value, ConfigLocation, ConfigSet};
use grit_lib::error::ConfigError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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
pub fn run(
    global: bool,
    local: bool,
    file: Option<PathBuf>,
    list: bool,
    unset: bool,
    key: Option<String>,
    value: Option<String>,
) -> Result<ConfigOutcome> {
    let write_location = resolve_location(global, local, file.clone(), true)?;
    let read_location = resolve_location(global, local, file, false)?;

    if list {
        if key.is_some() || value.is_some() {
            bail!("--list does not take a key or value");
        }
        return list_values(read_location);
    }

    let Some(key) = key else {
        bail!("specify a config key (e.g. user.name), or pass --list to see everything");
    };

    if unset {
        if value.is_some() {
            bail!("--unset does not take a value");
        }
        return unset_value(write_location, &key);
    }

    match value {
        Some(value) => set_value(write_location, &key, &value),
        None => get_value(read_location, &key),
    }
}

/// Set `key` to `value` in the global config (used by `grit auth` to wire up the
/// Windows credential helper).
#[cfg(windows)]
pub fn set_global(key: &str, value: &str) -> Result<()> {
    set_value(Some(ConfigLocation::Global), key, value).map(|_| ())
}

fn get_value(read_location: Option<ConfigLocation>, key: &str) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location)?;
    let origin = config
        .get_with_origin(key)
        .with_context(|| format!("invalid config key {key}"))?;
    Ok(match origin {
        Some(o) => ConfigOutcome::Get {
            key: key.to_owned(),
            value: o.value,
            scope: Some(o.scope.to_string()),
            file: o.file.as_ref().map(|p| p.display().to_string()),
            line: Some(o.line as u64),
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

fn list_values(read_location: Option<ConfigLocation>) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location)?;
    let entries = config
        .entries()
        .iter()
        .map(|entry| ConfigEntry {
            key: entry.key.clone(),
            value: entry.value.clone(),
            scope: entry.scope.to_string(),
            file: entry.file.as_ref().map(|p| p.display().to_string()),
        })
        .collect();
    Ok(ConfigOutcome::List { entries })
}

fn set_value(
    write_location: Option<ConfigLocation>,
    key: &str,
    value: &str,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let env = context::environment();
    let git_dir = context::discover().ok().map(|r| r.git_dir().to_path_buf());
    set_config_value(location, &env, git_dir.as_deref(), key, value)
        .with_context(|| format!("could not set {key}"))?;
    Ok(ConfigOutcome::Set {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

fn unset_value(write_location: Option<ConfigLocation>, key: &str) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let env = context::environment();
    let git_dir = context::discover().ok().map(|r| r.git_dir().to_path_buf());
    match unset_config_value(location, &env, git_dir.as_deref(), key) {
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
fn load_for_read(read_location: Option<ConfigLocation>) -> Result<ConfigSet> {
    let env = context::environment();
    if let Some(location) = read_location {
        let git_dir = context::discover().ok().map(|r| r.git_dir().to_path_buf());
        return location
            .load_single(&env, git_dir.as_deref())
            .context("could not load config");
    }
    if let Ok(repo) = context::discover() {
        ConfigSet::load(&env, Some(repo.git_dir()), true).context("could not load config")
    } else {
        ConfigSet::load(&env, None, true).context("could not load config")
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

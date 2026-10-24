//! `grit config` — read, set, and list configuration values.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use grit_lib::config::environment::Environment;
use grit_lib::config::{
    add_config_value, canonical_key, canonicalize_config_write_value, remove_config_section,
    rename_config_section, set_config_value, unset_all_config_values, unset_config_value,
    ConfigEntry as LibConfigEntry, ConfigLocation, ConfigScope, ConfigSet, ConfigValueType,
    ConfigWritePreview,
};
use grit_lib::error::ConfigError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

/// CLI mapping for `--type`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ConfigTypeArg {
    Bool,
    Int,
    Path,
}

impl From<ConfigTypeArg> for ConfigValueType {
    fn from(value: ConfigTypeArg) -> Self {
        match value {
            ConfigTypeArg::Bool => Self::Bool,
            ConfigTypeArg::Int => Self::Int,
            ConfigTypeArg::Path => Self::Path,
        }
    }
}

/// One value in a `get_all` outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct ConfigValueWithOrigin {
    pub value: Option<String>,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

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
    GetAll {
        key: String,
        values: Vec<ConfigValueWithOrigin>,
    },
    List {
        entries: Vec<ConfigEntry>,
    },
    Set {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Add {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Unset {
        key: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    UnsetAll {
        key: String,
        removed: u64,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    RenameSection {
        old_name: String,
        new_name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    RemoveSection {
        section: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// One entry in a `list` outcome. `value` is `null` for a bare/valueless key.
#[derive(Serialize, JsonSchema)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
    pub scope: String,
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
            ConfigOutcome::GetAll { values, .. } => {
                for v in values {
                    if let Some(value) = &v.value {
                        outln!(w, "{value}");
                    }
                }
            }
            ConfigOutcome::List { entries } => {
                for entry in entries {
                    match &entry.value {
                        Some(value) => outln!(w, "{}={value}", entry.key),
                        None => outln!(w, "{}", entry.key),
                    }
                }
            }
            ConfigOutcome::Set {
                preview, dry_run, ..
            }
            | ConfigOutcome::Add {
                preview, dry_run, ..
            }
            | ConfigOutcome::Unset {
                preview, dry_run, ..
            }
            | ConfigOutcome::UnsetAll {
                preview, dry_run, ..
            }
            | ConfigOutcome::RenameSection {
                preview, dry_run, ..
            }
            | ConfigOutcome::RemoveSection {
                preview, dry_run, ..
            } => {
                if *dry_run {
                    if let Some(p) = preview {
                        outln!(w, "{p}");
                    }
                }
            }
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
            ConfigOutcome::GetAll { key, values } => {
                outln!(w, "# Config `{key}` (all values)");
                outln!(w);
                for v in values {
                    outln!(
                        w,
                        "- `{}` ({})",
                        v.value.as_deref().unwrap_or("true"),
                        v.scope
                    );
                }
            }
            ConfigOutcome::List { entries } => markdown::print_config_entries(w, entries)?,
            ConfigOutcome::Set {
                key,
                value,
                dry_run,
                preview,
            }
            | ConfigOutcome::Add {
                key,
                value,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would set `{key}` = {value}. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Set `{key}` = {value}.");
                }
            }
            ConfigOutcome::Unset {
                key,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would unset `{key}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Unset `{key}`.");
                }
            }
            ConfigOutcome::UnsetAll {
                key,
                removed,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: would unset-all `{key}` ({removed} values). {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Unset all `{key}` ({removed} values).");
                }
            }
            ConfigOutcome::RenameSection {
                old_name,
                new_name,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: rename `{old_name}` → `{new_name}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Renamed section `{old_name}` → `{new_name}`.");
                }
            }
            ConfigOutcome::RemoveSection {
                section,
                dry_run,
                preview,
            } => {
                if *dry_run {
                    outln!(
                        w,
                        "Dry-run: remove section `{section}`. {}",
                        preview.as_deref().unwrap_or("")
                    );
                } else {
                    outln!(w, "Removed section `{section}`.");
                }
            }
        }
        Ok(())
    }
}

/// Exit code for `get` / `get_all` when no value is found (`1`); otherwise `0`.
#[must_use]
pub fn exit_code(outcome: &ConfigOutcome) -> i32 {
    match outcome {
        ConfigOutcome::Get { value: None, .. } => 1,
        ConfigOutcome::GetAll { values, .. } if values.is_empty() => 1,
        _ => 0,
    }
}

/// Run `grit config`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    _process_env: &Environment,
    global: bool,
    local: bool,
    system: bool,
    worktree: bool,
    file: Option<PathBuf>,
    list: bool,
    unset: bool,
    unset_all: bool,
    add: bool,
    get_all: bool,
    value_type: Option<ConfigTypeArg>,
    default_value: Option<String>,
    rename_section: Option<(String, String)>,
    remove_section: Option<String>,
    dry_run: bool,
    key: Option<String>,
    value: Option<String>,
    mode: OutputMode,
) -> Result<ConfigOutcome> {
    let write_action = unset
        || unset_all
        || add
        || value.is_some()
        || rename_section.is_some()
        || remove_section.is_some();
    if dry_run && !write_action {
        bail!(
            "--dry-run requires a config change (set a value, --unset, --add, or a section change)"
        );
    }

    if list {
        if value.is_some() {
            bail!("--list does not take a value");
        }
        let env = context::environment();
        let read_location = resolve_location(global, local, system, worktree, file, false)?;
        return list_values(read_location, &env, key.as_deref());
    }

    if let Some((old, new)) = rename_section {
        if key.is_some() || value.is_some() || unset || unset_all || add || get_all {
            bail!("--rename-section does not take a key or value");
        }
        let write_location = resolve_location(global, local, system, worktree, file.clone(), true)?;
        return rename_section_cmd(write_location, &context::environment(), &old, &new, dry_run);
    }

    if let Some(section) = remove_section {
        if key.is_some() || value.is_some() || unset || unset_all || add || get_all {
            bail!("--remove-section does not take a key or value");
        }
        let write_location = resolve_location(global, local, system, worktree, file.clone(), true)?;
        return remove_section_cmd(write_location, &context::environment(), &section, dry_run);
    }

    let Some(key) = key else {
        bail!("specify a config key (e.g. user.name), or pass --list to see everything");
    };

    if default_repo_hot_path(
        global,
        local,
        system,
        worktree,
        file.as_ref(),
        mode,
        dry_run,
    ) && !get_all
        && !unset_all
        && !add
        && value_type.is_none()
        && default_value.is_none()
    {
        let env = context::environment();
        if unset {
            if value.is_some() {
                bail!("--unset does not take a value");
            }
            return hot_unset_in_repo(&key, dry_run);
        }
        if let Some(value) = value {
            return hot_set_in_repo(&key, &value, dry_run);
        }
        return hot_get_in_repo(&env, &key);
    }

    let write_location = resolve_location(global, local, system, worktree, file.clone(), true)?;
    let read_location = resolve_location(global, local, system, worktree, file, false)?;
    let env = context::environment();

    if add && value.is_none() {
        bail!("--add requires a value");
    }

    if get_all {
        if value.is_some() {
            bail!("--all does not take a value");
        }
        return get_all_values(read_location, &env, &key, value_type, default_value, mode);
    }

    if unset_all {
        if value.is_some() || add {
            bail!("--unset-all does not take a value");
        }
        return unset_all_cmd(write_location, &env, &key, dry_run);
    }

    if unset {
        if value.is_some() {
            bail!("--unset does not take a value");
        }
        return unset_value(write_location, &env, &key, dry_run);
    }

    if add {
        let Some(value) = value else {
            bail!("--add requires a value");
        };
        let stored = prepare_write_value(&key, value_type, &value, &env)?;
        return add_value(write_location, &env, &key, &stored, dry_run);
    }

    if let Some(value) = value {
        let stored = prepare_write_value(&key, value_type, &value, &env)?;
        return set_value(write_location, &env, &key, &stored, dry_run);
    }

    get_value(read_location, &env, &key, value_type, default_value, mode)
}

fn preview_text(preview: &Option<ConfigWritePreview>) -> Option<String> {
    preview.as_ref().map(|p| p.summary.clone())
}

fn prepare_write_value(
    key: &str,
    value_type: Option<ConfigTypeArg>,
    value: &str,
    env: &grit_lib::config::environment::Environment,
) -> Result<String> {
    match value_type {
        Some(ty) => canonicalize_config_write_value(key, ty.into(), value, env)
            .with_context(|| format!("could not canonicalize value for {key}")),
        None => Ok(value.to_owned()),
    }
}

fn default_repo_hot_path(
    global: bool,
    local: bool,
    system: bool,
    worktree: bool,
    file: Option<&PathBuf>,
    mode: OutputMode,
    dry_run: bool,
) -> bool {
    !dry_run
        && !global
        && !local
        && !system
        && !worktree
        && file.is_none()
        && matches!(mode, OutputMode::Human)
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

fn hot_set_in_repo(key: &str, value: &str, dry_run: bool) -> Result<ConfigOutcome> {
    let repo = context::discover()?;
    let env = context::environment();
    let git_dir = repo.git_dir().to_path_buf();
    let preview = set_config_value(
        ConfigLocation::Local,
        &env,
        Some(&git_dir),
        key,
        value,
        dry_run,
    )
    .with_context(|| format!("could not set {key}"))?;
    Ok(ConfigOutcome::Set {
        key: key.to_owned(),
        value: value.to_owned(),
        dry_run,
        preview: preview_text(&preview),
    })
}

fn hot_unset_in_repo(key: &str, dry_run: bool) -> Result<ConfigOutcome> {
    let repo = context::discover()?;
    let env = context::environment();
    let git_dir = repo.git_dir().to_path_buf();
    match unset_config_value(ConfigLocation::Local, &env, Some(&git_dir), key, dry_run) {
        Ok(preview) => Ok(ConfigOutcome::Unset {
            key: key.to_owned(),
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(grit_lib::error::Error::Config(ConfigError::KeyNotSet { .. })) => {
            bail!("key '{key}' is not set");
        }
        Err(e) => Err(e.into()),
    }
}

fn read_merged_config(env: &grit_lib::config::environment::Environment) -> Result<ConfigSet> {
    if let Ok(repo) = context::discover() {
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")
    } else {
        ConfigSet::load(env, None, true).context("could not load config")
    }
}

/// Set `key` to `value` in the global (per-user) config file.
pub fn set_global(key: &str, value: &str) -> Result<()> {
    set_value(
        Some(ConfigLocation::Global),
        &context::environment(),
        key,
        value,
        false,
    )
    .map(|_| ())
}

fn map_origin(o: grit_lib::config::ConfigValueOrigin) -> ConfigValueWithOrigin {
    ConfigValueWithOrigin {
        value: o.value,
        scope: o.scope.to_string(),
        file: o.file.as_ref().map(|p| p.display().to_string()),
        line: o.line.map(|l| l as u64),
    }
}

fn get_value(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    value_type: Option<ConfigTypeArg>,
    default_value: Option<String>,
    mode: OutputMode,
) -> Result<ConfigOutcome> {
    canonical_key(key).with_context(|| format!("invalid config key {key}"))?;
    let config = load_for_read(read_location, env)?;

    if let Some(ty) = value_type {
        let Some(raw) = resolve_get_raw(&config, key, default_value.as_deref())? else {
            canonical_key(key).context(format!("invalid config key {key}"))?;
            return Ok(ConfigOutcome::Get {
                key: key.to_owned(),
                value: None,
                scope: None,
                file: None,
                line: None,
            });
        };
        let typed = grit_lib::config::parse_typed_value(key, ty.into(), &raw, env)?;
        let value = typed.to_canonical_string();
        if matches!(mode, OutputMode::Human) {
            return Ok(ConfigOutcome::Get {
                key: key.to_owned(),
                value: Some(value),
                scope: None,
                file: None,
                line: None,
            });
        }
        let origin = config
            .get_with_origin(key)
            .with_context(|| format!("invalid config key {key}"))?;
        return Ok(match origin {
            Some(o) => ConfigOutcome::Get {
                key: key.to_owned(),
                value: Some(value),
                scope: Some(o.scope.to_string()),
                file: o.file.as_ref().map(|p| p.display().to_string()),
                line: o.line.map(|l| l as u64),
            },
            None => ConfigOutcome::Get {
                key: key.to_owned(),
                value: Some(value),
                scope: None,
                file: None,
                line: None,
            },
        });
    }

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
                if let Some(def) = default_value {
                    return Ok(ConfigOutcome::Get {
                        key: key.to_owned(),
                        value: Some(def),
                        scope: None,
                        file: None,
                        line: None,
                    });
                }
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
            value: default_value,
            scope: None,
            file: None,
            line: None,
        },
    })
}

fn resolve_get_raw(
    config: &ConfigSet,
    key: &str,
    default_value: Option<&str>,
) -> Result<Option<String>> {
    if let Some(v) = config.get(key) {
        return Ok(Some(v));
    }
    Ok(default_value.map(str::to_owned))
}

fn get_all_values(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    value_type: Option<ConfigTypeArg>,
    default_value: Option<String>,
    _mode: OutputMode,
) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location, env)?;
    let mut origins = config.get_all_with_origin(key)?;
    if origins.is_empty() {
        if let Some(def) = default_value {
            origins.push(grit_lib::config::ConfigValueOrigin {
                value: Some(def),
                scope: ConfigScope::Command,
                file: None,
                line: None,
            });
        }
    }
    let values: Vec<ConfigValueWithOrigin> = if let Some(ty) = value_type {
        origins
            .into_iter()
            .map(|o| {
                let raw = o.value.as_deref().unwrap_or("true");
                let typed = grit_lib::config::parse_typed_value(key, ty.into(), raw, env)?;
                Ok(ConfigValueWithOrigin {
                    value: Some(typed.to_canonical_string()),
                    scope: o.scope.to_string(),
                    file: o.file.as_ref().map(|p| p.display().to_string()),
                    line: o.line.map(|l| l as u64),
                })
            })
            .collect::<Result<_>>()?
    } else {
        origins.into_iter().map(map_origin).collect()
    };
    Ok(ConfigOutcome::GetAll {
        key: key.to_owned(),
        values,
    })
}

fn list_values(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    prefix: Option<&str>,
) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location, env)?;
    let lib_entries: Vec<LibConfigEntry> = if let Some(prefix) = prefix {
        config.entries_with_prefix(prefix)?
    } else {
        config.entries().to_vec()
    };
    let entries = lib_entries
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
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    let preview = set_config_value(location, env, git_dir.as_deref(), key, value, dry_run)
        .with_context(|| format!("could not set {key}"))?;
    Ok(ConfigOutcome::Set {
        key: key.to_owned(),
        value: value.to_owned(),
        dry_run,
        preview: preview_text(&preview),
    })
}

fn add_value(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    value: &str,
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    let preview = add_config_value(location, env, git_dir.as_deref(), key, value, dry_run)
        .with_context(|| format!("could not add {key}"))?;
    Ok(ConfigOutcome::Add {
        key: key.to_owned(),
        value: value.to_owned(),
        dry_run,
        preview: preview_text(&preview),
    })
}

fn unset_value(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    match unset_config_value(location, env, git_dir.as_deref(), key, dry_run) {
        Ok(preview) => Ok(ConfigOutcome::Unset {
            key: key.to_owned(),
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(grit_lib::error::Error::Config(ConfigError::KeyNotSet { .. })) => {
            bail!("key '{key}' is not set");
        }
        Err(e) => Err(e.into()),
    }
}

fn unset_all_cmd(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    match unset_all_config_values(location, env, git_dir.as_deref(), key, dry_run) {
        Ok((removed, preview)) => Ok(ConfigOutcome::UnsetAll {
            key: key.to_owned(),
            removed: removed as u64,
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(grit_lib::error::Error::Config(ConfigError::KeyNotSet { .. })) => {
            bail!("key '{key}' is not set");
        }
        Err(e) => Err(e.into()),
    }
}

fn rename_section_cmd(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    old: &str,
    new: &str,
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    let preview = rename_config_section(location, env, git_dir.as_deref(), old, new, dry_run)?;
    Ok(ConfigOutcome::RenameSection {
        old_name: old.to_owned(),
        new_name: new.to_owned(),
        dry_run,
        preview: preview_text(&preview),
    })
}

fn remove_section_cmd(
    write_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    section: &str,
    dry_run: bool,
) -> Result<ConfigOutcome> {
    let Some(location) = write_location else {
        bail!("internal error: write location missing");
    };
    let git_dir = git_dir_for_location(&location)?;
    let preview = remove_config_section(location, env, git_dir.as_deref(), section, dry_run)?;
    Ok(ConfigOutcome::RemoveSection {
        section: section.to_owned(),
        dry_run,
        preview: preview_text(&preview),
    })
}

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
    system: bool,
    worktree: bool,
    file: Option<PathBuf>,
    for_write: bool,
) -> Result<Option<ConfigLocation>> {
    let flags = usize::from(global)
        + usize::from(local)
        + usize::from(system)
        + usize::from(worktree)
        + usize::from(file.is_some());
    if flags > 1 {
        bail!("--global, --local, --system, --worktree, and --file are mutually exclusive");
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
    if system {
        return Ok(Some(ConfigLocation::System));
    }
    if worktree {
        return Ok(Some(ConfigLocation::Worktree));
    }
    if for_write {
        Ok(Some(ConfigLocation::Local))
    } else {
        Ok(None)
    }
}

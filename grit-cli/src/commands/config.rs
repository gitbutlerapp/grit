//! `grit config` — read, set, and list configuration values.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use grit_lib::config::environment::Environment;
use grit_lib::config::git_path::user_display_path;
use grit_lib::config::{
    canonical_key, canonicalize_config_write_value, ConfigEditor, ConfigEntry as LibConfigEntry,
    ConfigLocation, ConfigScope, ConfigSet, ConfigValueType, ConfigWriteOptions,
    ConfigWritePreview,
};
use grit_lib::repo::Repository;

use crate::context;
pub use crate::render::config::*;

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
    human: bool,
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
        human,
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
        return get_all_values(read_location, &env, &key, value_type, default_value);
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

    get_value(read_location, &env, &key, value_type, default_value, human)
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
    human: bool,
    dry_run: bool,
) -> bool {
    !dry_run && !global && !local && !system && !worktree && file.is_none() && human
}

fn hot_get_in_repo(
    env: &grit_lib::config::environment::Environment,
    key: &str,
) -> Result<ConfigOutcome> {
    let config = read_merged_config(env)?;
    match crate::context::config_string(&config, key) {
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
    let options = ConfigWriteOptions::new().dry_run(dry_run);
    let preview = repo
        .config()
        .set(ConfigScope::Local, key, value, &options)
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
    let options = ConfigWriteOptions::new().dry_run(dry_run);
    match repo.config().unset(ConfigScope::Local, key, &options) {
        Ok(preview) => Ok(ConfigOutcome::Unset {
            key: key.to_owned(),
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(e) => Err(crate::error_map::map_config_unset_error(key, e)),
    }
}

fn read_merged_config(env: &grit_lib::config::environment::Environment) -> Result<ConfigSet> {
    if let Ok((_, config)) = context::locate_repository_with_config() {
        Ok(config)
    } else {
        ConfigSet::load(
            env,
            None,
            &grit_lib::config::LoadConfigOptions::standard(env, None),
        )
        .context("could not load config")
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

fn write_options(dry_run: bool) -> ConfigWriteOptions {
    ConfigWriteOptions::new().dry_run(dry_run)
}

fn with_write_target<R>(
    location: ConfigLocation,
    env: &Environment,
    repo_op: impl FnOnce(&Repository, ConfigScope) -> grit_lib::error::Result<R>,
    editor_op: impl FnOnce(&ConfigEditor) -> grit_lib::error::Result<R>,
) -> grit_lib::error::Result<R> {
    match location {
        ConfigLocation::File(path) => {
            let editor = ConfigEditor::open(env, path, ConfigScope::Local);
            editor_op(&editor)
        }
        ConfigLocation::Global | ConfigLocation::System => {
            if let Ok(repo) = context::discover() {
                let scope = match location {
                    ConfigLocation::Global => ConfigScope::Global,
                    ConfigLocation::System => ConfigScope::System,
                    _ => unreachable!(),
                };
                repo_op(&repo, scope)
            } else {
                let editor = match location {
                    ConfigLocation::Global => ConfigEditor::global(env)?,
                    ConfigLocation::System => ConfigEditor::system(env),
                    _ => unreachable!(),
                };
                editor_op(&editor)
            }
        }
        // `Local`, `Worktree` and any repository-bound location added later.
        _ => {
            let repo = crate::error_map::discover_for_config_write()?;
            let scope = match location {
                ConfigLocation::Worktree => ConfigScope::Worktree,
                _ => ConfigScope::Local,
            };
            repo_op(&repo, scope)
        }
    }
}

fn map_origin(o: grit_lib::config::ConfigValueOrigin) -> ConfigValueWithOrigin {
    ConfigValueWithOrigin {
        value: o.value,
        scope: o.scope.to_string(),
        file: o.file.as_ref().map(|p| user_display_path(p)),
        line: o.line.map(|l| l as u64),
    }
}

fn get_value(
    read_location: Option<ConfigLocation>,
    env: &grit_lib::config::environment::Environment,
    key: &str,
    value_type: Option<ConfigTypeArg>,
    default_value: Option<String>,
    human: bool,
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
        if human {
            return Ok(ConfigOutcome::Get {
                key: key.to_owned(),
                value: Some(value),
                scope: None,
                file: None,
                line: None,
            });
        }
        let origin = config
            .origin_of(key)
            .with_context(|| format!("invalid config key {key}"))?;
        return Ok(match origin {
            Some(o) => ConfigOutcome::Get {
                key: key.to_owned(),
                value: Some(value),
                scope: Some(o.scope.to_string()),
                file: o.file.as_ref().map(|p| user_display_path(p)),
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

    if human {
        match crate::context::config_string(&config, key) {
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
        .origin_of(key)
        .with_context(|| format!("invalid config key {key}"))?;
    Ok(match origin {
        Some(o) => ConfigOutcome::Get {
            key: key.to_owned(),
            value: o.value,
            scope: Some(o.scope.to_string()),
            file: o.file.as_ref().map(|p| user_display_path(p)),
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
    if let Some(v) = crate::context::config_string(config, key) {
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
) -> Result<ConfigOutcome> {
    let config = load_for_read(read_location, env)?;
    let mut origins = config.origins_of(key)?;
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
                    file: o.file.as_ref().map(|p| user_display_path(p)),
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
        config.entries_under(prefix)?
    } else {
        config.entries().to_vec()
    };
    let entries = lib_entries
        .iter()
        .map(|entry| ConfigEntry {
            key: entry.key.clone(),
            value: entry.value.clone(),
            scope: entry.scope.to_string(),
            file: grit_lib::config::config_entry_origin_file(entry).map(user_display_path),
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
    let options = write_options(dry_run);
    let preview = with_write_target(
        location,
        env,
        |repo, scope| repo.config().set(scope, key, value, &options),
        |editor| editor.set(key, value, &options),
    )
    .map_err(anyhow::Error::from)
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
    let options = write_options(dry_run);
    let preview = with_write_target(
        location,
        env,
        |repo, scope| repo.config().add_value(scope, key, value, &options),
        |editor| editor.add_value(key, value, &options),
    )
    .map_err(anyhow::Error::from)
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
    let options = write_options(dry_run);
    match with_write_target(
        location,
        env,
        |repo, scope| repo.config().unset(scope, key, &options),
        |editor| editor.unset(key, &options),
    ) {
        Ok(preview) => Ok(ConfigOutcome::Unset {
            key: key.to_owned(),
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(e) => Err(crate::error_map::map_config_unset_error(key, e)),
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
    let options = write_options(dry_run);
    match with_write_target(
        location,
        env,
        |repo, scope| repo.config().unset_all(scope, key, &options),
        |editor| editor.unset_all(key, &options),
    ) {
        Ok((removed, preview)) => Ok(ConfigOutcome::UnsetAll {
            key: key.to_owned(),
            removed: removed as u64,
            dry_run,
            preview: preview_text(&preview),
        }),
        Err(e) => Err(crate::error_map::map_config_unset_error(key, e)),
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
    let options = write_options(dry_run);
    let preview = with_write_target(
        location,
        env,
        |repo, scope| repo.config().rename_section(scope, old, new, &options),
        |editor| editor.rename_section(old, new, &options),
    )
    .map_err(anyhow::Error::from)?;
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
    let options = write_options(dry_run);
    let preview = with_write_target(
        location,
        env,
        |repo, scope| repo.config().remove_section(scope, section, &options),
        |editor| editor.remove_section(section, &options),
    )
    .map_err(anyhow::Error::from)?;
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
        ConfigLocation::Global | ConfigLocation::System | ConfigLocation::File(_) => Ok(None),
        // `Local`, `Worktree` and any repository-bound location added later.
        _ => Ok(Some(context::locate_repository()?.git_dir().to_path_buf())),
    }
}

fn optional_git_dir_for_read(location: &ConfigLocation) -> Result<Option<PathBuf>> {
    match location {
        ConfigLocation::Global | ConfigLocation::System | ConfigLocation::File(_) => {
            Ok(context::locate_repository()
                .ok()
                .map(|r| r.git_dir().to_path_buf()))
        }
        _ => git_dir_for_location(location),
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

/// Dispatch parsed CLI args for `Config`.
pub fn dispatch(
    args: crate::args::ConfigArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::CommandEnv;
    use crate::exit_status::ExitStatus;

    use crate::output::emit;

    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env,
    } = env;
    let ConfigArgs {
        global,
        local,
        system,
        worktree,
        file,
        list,
        all,
        add,
        unset,
        unset_all,
        rename_section,
        rename_section_new,
        remove_section,
        value_type,
        default_value,
        dry_run,
        key,
        value,
    } = args;
    {
        let rename_pair = match (rename_section, rename_section_new) {
            (Some(old), Some(new)) => Some((old, new)),
            (None, None) => None,
            _ => {
                bail!("--rename-section requires both OLD and --rename-section-new NEW");
            }
        };
        let show_progress = opts.show_progress();
        let outcome = commands::config::run(
            process_env,
            global,
            local,
            system,
            worktree,
            file,
            list,
            unset,
            unset_all,
            add,
            all,
            value_type,
            default_value,
            rename_pair,
            remove_section,
            dry_run,
            key,
            value,
            show_progress,
        )?;
        let code = commands::config::exit_code(&outcome);
        emit(outcome, opts, ctx)?;
        if code != 0 {
            std::process::exit(code);
        }
        Ok(ExitStatus::Success)
    }
}

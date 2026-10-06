//! `grit config` — read and modify Git configuration files.
//!
//! Supports both the legacy interface (`git config --get`, `git config key value`)
//! and the new subcommand interface (`git config get`, `git config set`).

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args as ClapArgs, Subcommand};
use grit_lib::config::{
    parse_bool, parse_color, parse_i64, ConfigFile, ConfigIncludeOrigin, ConfigScope, ConfigSet,
    IncludeContext, LoadConfigOptions,
};
use grit_lib::error::Error as LibError;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{common_git_dir_for_config, worktree_config_enabled, Repository};
use grit_lib::rev_parse::resolve_revision;
use grit_lib::worktree::registered_worktree_count;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigValueType {
    Bool,
    Int,
    BoolOrInt,
    Path,
    ExpiryDate,
    Color,
}

impl ConfigValueType {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "bool" => Some(Self::Bool),
            "int" => Some(Self::Int),
            "bool-or-int" => Some(Self::BoolOrInt),
            "path" => Some(Self::Path),
            "expiry-date" => Some(Self::ExpiryDate),
            "color" => Some(Self::Color),
            _ => None,
        }
    }
}

/// True when `--bool`, `--bool-or-int`, or `--type=bool|bool-or-int` requests explicit boolean output.
fn regexp_type_requests_bool_output(args: &Args) -> bool {
    type_is(args, ConfigValueType::Bool) || type_is(args, ConfigValueType::BoolOrInt)
}

/// Arguments for `grit config`.
#[derive(Debug, ClapArgs)]
#[command(
    about = "Get and set repository or global options",
    after_help = "Use subcommands (get, set, unset, list) or legacy flags (--get, key value).",
    allow_negative_numbers = true
)]
pub struct Args {
    /// Run as if started in this directory (repeatable).
    ///
    /// Declared before `subcommand` so `git config -C <dir> key value` parses like Git.
    #[arg(short = 'C', value_name = "PATH", global = true)]
    pub change_dir: Vec<PathBuf>,

    #[command(subcommand)]
    pub subcommand: Option<ConfigSubcommand>,

    // ── File location flags ──
    /// Use the system-wide config file.
    #[arg(long, global = true)]
    pub system: bool,

    /// Use the global (per-user) config file.
    #[arg(long, global = true)]
    pub global: bool,

    /// Use the given git directory (affects repo discovery for local config).
    #[arg(long = "git-dir", value_name = "PATH", global = true)]
    pub git_dir_path: Option<PathBuf>,

    /// Use the repository-local config file.
    #[arg(long, global = true)]
    pub local: bool,

    /// Use the per-worktree config file.
    #[arg(long, global = true)]
    pub worktree: bool,

    /// Use the given config file.
    #[arg(short = 'f', long = "file", global = true)]
    pub file: Option<PathBuf>,

    /// Read config from a blob object (e.g. HEAD:.gitmodules).
    #[arg(long = "blob", value_name = "BLOB_ISH", global = true)]
    pub blob: Option<String>,

    // ── Legacy action flags ──
    /// Get the value for a given key (legacy).
    #[arg(long = "get", value_name = "KEY", num_args = 0..=1, default_missing_value = "")]
    pub get_key: Option<String>,

    /// Get all values for a multi-valued key (legacy).
    #[arg(long = "get-all", value_name = "KEY", num_args = 0..=1, default_missing_value = "")]
    pub get_all_key: Option<String>,

    /// Unsupported negated legacy get mode.
    #[arg(long = "no-get", hide = true)]
    pub no_get: bool,

    /// Get values matching a regex (legacy).
    #[arg(long = "get-regexp", value_name = "PATTERN", num_args = 0..=1, default_missing_value = "")]
    pub get_regexp: Option<String>,

    /// Remove a key (legacy).
    #[arg(long = "unset", value_name = "KEY", num_args = 0..=1, default_missing_value = "")]
    pub unset_key: Option<String>,

    /// Remove all occurrences of a key (legacy).
    #[arg(long = "unset-all", value_name = "KEY", num_args = 0..=1, default_missing_value = "")]
    pub unset_all_key: Option<String>,

    /// List all config entries (legacy).
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    /// Add a new line for a multi-valued key (legacy).
    ///
    /// Supports `git config --add -f path key value` (key may follow `-f` and the file path).
    #[arg(
        long = "add",
        value_name = "KEY",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub add_key: Option<String>,

    /// Replace all matching values (legacy).
    #[arg(long = "replace-all")]
    pub replace_all: bool,

    /// Append an inline comment to the value.
    #[arg(long = "comment", global = true)]
    pub comment: Option<String>,

    /// Rename a section (legacy).
    #[arg(long = "rename-section")]
    pub rename_section: bool,

    /// Remove a section (legacy).
    #[arg(long = "remove-section")]
    pub remove_section: bool,

    /// Open the config file in an editor (legacy).
    #[arg(long = "edit")]
    pub edit: bool,

    // ── Type flags ──
    /// Ensure the value is a valid boolean and canonicalize.
    #[arg(long = "bool", global = true, action = ArgAction::Count)]
    pub type_bool: u8,

    /// Ensure the value is a valid integer and canonicalize.
    #[arg(long = "int", global = true, action = ArgAction::Count)]
    pub type_int: u8,

    /// Ensure the value is a valid bool-or-int and canonicalize.
    #[arg(long = "bool-or-int", global = true, action = ArgAction::Count)]
    pub type_bool_or_int: u8,

    /// Expand `~/` in the value.
    #[arg(long = "path", global = true, action = ArgAction::Count)]
    pub type_path: u8,

    /// Interpret the value as an expiry date and print its timestamp.
    #[arg(long = "expiry-date", global = true, action = ArgAction::Count)]
    pub type_expiry_date: u8,

    /// Type selector (alternative to individual flags).
    #[arg(long = "type", value_name = "TYPE", global = true, action = ArgAction::Append)]
    pub type_name: Vec<String>,

    /// Clear any previously selected type.
    #[arg(long = "no-type", global = true, action = ArgAction::Count)]
    pub no_type: u8,

    // ── Display flags ──
    /// Show origin file and scope for each entry.
    #[arg(long = "show-origin", global = true)]
    pub show_origin: bool,

    /// Show scope for each entry.
    #[arg(long = "show-scope", global = true)]
    pub show_scope: bool,

    /// Use NUL as delimiter.
    #[arg(short = 'z', long = "null", global = true)]
    pub null_terminated: bool,

    /// Show key names for --get-regexp.
    #[arg(long = "name-only", global = true)]
    pub name_only: bool,

    /// Includes support.
    #[arg(long = "includes")]
    pub includes: bool,

    /// Do not honour include directives.
    #[arg(long = "no-includes")]
    pub no_includes: bool,

    /// Default value if key is not found (legacy --get/--get-all).
    #[arg(long = "default", value_name = "VALUE", global = true)]
    pub default_value: Option<String>,

    /// Only match exact values (instead of treating value as regex).
    #[arg(long = "fixed-value", global = true)]
    pub fixed_value: bool,

    // ── URL match flags ──
    /// Get the best-matching value for the given URL.
    #[arg(long = "get-urlmatch", value_name = "KEY", num_args = 0..=1, default_missing_value = "")]
    pub get_urlmatch_key: Option<String>,

    /// Get the color setting (legacy): returns ANSI code for the color, with default.
    #[arg(long = "get-color", value_name = "KEY", num_args = 1)]
    pub get_color_key: Option<String>,

    // ── Positional args for legacy set (`git config key value`) ──
    /// Positional arguments (key, value, value-pattern for legacy mode).
    ///
    /// `allow_negative_numbers` is required so `git config --get-color SLOT -1` treats `-1` as
    /// a default color (Git synonym for `normal`), not as a clap flag.
    #[arg(trailing_var_arg = true, allow_negative_numbers = true)]
    pub positional: Vec<String>,
}

fn legacy_type_specs(args: &Args) -> Vec<ConfigValueType> {
    let mut specs = Vec::new();
    if args.type_bool > 0 {
        specs.push(ConfigValueType::Bool);
    }
    if args.type_int > 0 {
        specs.push(ConfigValueType::Int);
    }
    if args.type_bool_or_int > 0 {
        specs.push(ConfigValueType::BoolOrInt);
    }
    if args.type_path > 0 {
        specs.push(ConfigValueType::Path);
    }
    if args.type_expiry_date > 0 {
        specs.push(ConfigValueType::ExpiryDate);
    }
    specs
}

fn type_name_specs(args: &Args) -> Result<Vec<ConfigValueType>> {
    args.type_name
        .iter()
        .map(|name| {
            ConfigValueType::parse(name).ok_or_else(|| {
                fatal_config_parse(format!("fatal: unrecognized --type argument, {name}"))
            })
        })
        .collect()
}

fn validate_type_specifiers(args: &Args) -> Result<()> {
    let mut specs = legacy_type_specs(args);
    specs.extend(type_name_specs(args)?);
    specs.dedup();
    if args.no_type == 0 && specs.len() > 1 {
        return Err(fatal_config_parse("fatal: only one type at a time"));
    }
    Ok(())
}

fn effective_type(args: &Args) -> Option<ConfigValueType> {
    if args.no_type > 0 {
        if args.type_name.len() > 1 {
            return args
                .type_name
                .last()
                .and_then(|name| ConfigValueType::parse(name));
        }
        return None;
    }

    type_name_specs(args)
        .ok()
        .and_then(|specs| specs.last().copied())
        .or_else(|| legacy_type_specs(args).last().copied())
}

fn type_is(args: &Args, expected: ConfigValueType) -> bool {
    effective_type(args) == Some(expected)
}

fn has_type(args: &Args) -> bool {
    effective_type(args).is_some()
}

fn reject_fixed_value(args: &Args) -> Result<()> {
    if args.fixed_value {
        bail!("error: --fixed-value only applies with a value-pattern");
    }
    Ok(())
}

fn require_fixed_value_pattern(args: &Args, value_pattern: Option<&str>) -> Result<()> {
    if args.fixed_value && value_pattern.is_none() {
        bail!("error: --fixed-value only applies with a value-pattern");
    }
    Ok(())
}

fn regex_api_value_pattern(args: &Args, value_pattern: Option<&str>) -> Option<String> {
    value_pattern.map(|pattern| {
        if args.fixed_value {
            regex::escape(pattern)
        } else {
            pattern.to_owned()
        }
    })
}

/// Modern subcommand interface for `grit config`.
#[derive(Debug, Subcommand)]
pub enum ConfigSubcommand {
    /// Get the value for a key.
    Get(GetArgs),
    /// Set a key to a value.
    Set(SetArgs),
    /// Unset (remove) a key.
    Unset(UnsetArgs),
    /// List all config entries.
    List(ListArgs),
    /// Rename a section.
    #[command(name = "rename-section")]
    RenameSection(RenameSectionArgs),
    /// Remove a section.
    #[command(name = "remove-section")]
    RemoveSection(RemoveSectionArgs),
    /// Open the config file in an editor.
    Edit(EditArgs),
}

/// Arguments for `grit config get`.
#[derive(Debug, ClapArgs)]
pub struct GetArgs {
    /// The configuration key.
    pub key: String,

    /// Get all values (multi-valued key).
    #[arg(long)]
    pub all: bool,

    /// Treat key as a regex.
    #[arg(long)]
    pub regexp: bool,

    /// Show key names alongside values.
    #[arg(long = "show-names")]
    pub show_names: bool,

    /// Default value if key is missing.
    #[arg(long)]
    pub default: Option<String>,

    /// Match config against a URL.
    #[arg(long = "url")]
    pub url: Option<String>,

    /// Only return values matching this pattern.
    #[arg(long = "value")]
    pub value: Option<String>,

    /// Show origin file and scope for each entry.
    #[arg(long = "show-origin")]
    pub show_origin: bool,

    /// Show scope for each entry.
    #[arg(long = "show-scope")]
    pub show_scope: bool,
}

/// Arguments for `grit config set`.
#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    /// The configuration key.
    pub key: String,
    /// The value to set.
    #[arg(allow_hyphen_values = true)]
    pub value: String,

    /// Replace all matching values.
    #[arg(long)]
    pub all: bool,

    /// Append a new line for a multi-valued key.
    #[arg(long)]
    pub append: bool,
}

/// Arguments for `grit config unset`.
#[derive(Debug, ClapArgs)]
pub struct UnsetArgs {
    /// The configuration key.
    pub key: String,

    /// Remove all occurrences.
    #[arg(long)]
    pub all: bool,

    /// Only unset values matching this pattern.
    #[arg(long = "value")]
    pub value: Option<String>,
}

/// Arguments for `grit config list`.
#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Show only names, not values.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Show config file path.
    #[arg(long = "show-origin")]
    pub show_origin: bool,

    /// Show config scope.
    #[arg(long = "show-scope")]
    pub show_scope: bool,
}

/// Arguments for `grit config rename-section`.
#[derive(Debug, ClapArgs)]
pub struct RenameSectionArgs {
    /// Old section name.
    pub old_name: String,
    /// New section name.
    pub new_name: String,
}

/// Arguments for `grit config remove-section`.
#[derive(Debug, ClapArgs)]
pub struct RemoveSectionArgs {
    /// Section name to remove.
    pub name: String,
}

/// Arguments for `grit config edit`.
#[derive(Debug, ClapArgs)]
pub struct EditArgs {}

// ── Entrypoint ──────────────────────────────────────────────────────

/// Run the `config` command.
pub fn run(args: Args) -> Result<()> {
    validate_type_specifiers(&args)?;
    if args.no_get {
        bail!("unknown option `no-get'");
    }
    if args.get_key.is_some() && args.get_all_key.is_some() {
        bail!("options '--get-all' and '--get' cannot be used together");
    }

    for dir in &args.change_dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("cannot change to '{}'", dir.display()))?;
    }
    if let Some(ref p) = args.git_dir_path {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            std::env::current_dir()?.join(p)
        };
        std::env::set_current_dir(&abs)
            .with_context(|| format!("cannot change to directory '{}'", abs.display()))?;
    }

    // If --blob is given, read config from the blob and handle read-only ops
    if let Some(ref blob_spec) = args.blob {
        // --blob is incompatible with file-scope flags
        if args.system || args.global || args.local || args.worktree || args.file.is_some() {
            bail!("--blob and file-location options (--system, --global, --local, --worktree, --file) are incompatible");
        }
        return cmd_blob(&args, blob_spec);
    }

    if args.default_value.is_some() && !default_supported(&args) {
        bail!("--default is only applicable to --get, --get-all, --get-regexp, and lookup forms");
    }

    // Resolve which file to operate on
    let mut git_dir = resolve_git_dir();

    // Mirror git/setup.c `check_repository_format_gently`: `git config` runs with
    // `RUN_SETUP_GENTLY`, so when setup discovers a repository whose `config` declares an
    // unsupported format version (or unsupported extensions), git prints the message as a
    // `warning:` and then proceeds as if no repository were present (`have_repository = 0`).
    // Operations that read/write the local repo config therefore fail (exit 1), while
    // `--file`/`--global`/`--system` operations still succeed using their explicit files.
    if let Some(ref gd) = git_dir {
        if let Ok(Some(message)) = grit_lib::repo::repository_format_warning(gd) {
            eprintln!("warning: {message}");
            git_dir = None;
        }
    }

    let (scope, file_path) = resolve_config_file(&args, git_dir.as_deref())?;

    // Handle subcommands first
    if let Some(ref sub) = args.subcommand {
        return match sub {
            ConfigSubcommand::Get(get_args) => cmd_get(
                &args,
                get_args,
                git_dir.as_deref(),
                get_args.value.as_deref(),
            ),
            ConfigSubcommand::Set(set_args) => cmd_set(&args, set_args, scope, &file_path, None),
            ConfigSubcommand::Unset(unset_args) => {
                cmd_unset(
                    &args,
                    unset_args,
                    scope,
                    &file_path,
                    unset_args.value.as_deref(),
                    /* preserve_empty_section_header_on_unset_all */ false,
                )
            }
            ConfigSubcommand::List(list_args) => {
                reject_fixed_value(&args)?;
                // Merge list-level flags into top-level args
                let mut merged = Args {
                    name_only: args.name_only || list_args.name_only,
                    show_origin: args.show_origin || list_args.show_origin,
                    show_scope: args.show_scope || list_args.show_scope,
                    ..args
                };
                merged.subcommand = None; // avoid borrow issues
                cmd_list(&merged, git_dir.as_deref())
            }
            ConfigSubcommand::RenameSection(rs) => {
                reject_fixed_value(&args)?;
                cmd_rename_section(scope, &file_path, &rs.old_name, &rs.new_name)
            }
            ConfigSubcommand::RemoveSection(rs) => {
                reject_fixed_value(&args)?;
                cmd_remove_section(scope, &file_path, &rs.name)
            }
            ConfigSubcommand::Edit(_) => {
                reject_fixed_value(&args)?;
                cmd_edit(&file_path)
            }
        };
    }

    // Legacy interface
    if args.edit {
        reject_fixed_value(&args)?;
        return cmd_edit(&file_path);
    }

    if args.list {
        reject_fixed_value(&args)?;
        return cmd_list(&args, git_dir.as_deref());
    }

    if let Some(ref key_raw) = args.get_key {
        // When --get is used without an inline value (e.g. `--get --path a.key`),
        // the key comes from the first positional argument.
        let (key, value_pattern) = if key_raw.is_empty() {
            let k = args.positional.first().cloned().unwrap_or_default();
            let vp = args.positional.get(1).map(|s| s.as_str());
            (k, vp)
        } else {
            (key_raw.clone(), args.positional.first().map(|s| s.as_str()))
        };
        if key.is_empty() {
            bail!("usage: git config --get <key>");
        }
        let get_args = GetArgs {
            key,
            all: false,
            regexp: false,
            show_names: false,
            default: args.default_value.clone(),
            url: None,
            value: None,
            show_origin: false,
            show_scope: false,
        };
        return cmd_get(&args, &get_args, git_dir.as_deref(), value_pattern);
    }

    if let Some(ref key_raw) = args.get_all_key {
        let (key, value_pattern) = if key_raw.is_empty() {
            let k = args.positional.first().cloned().unwrap_or_default();
            let vp = args.positional.get(1).map(|s| s.as_str());
            (k, vp)
        } else {
            (key_raw.clone(), args.positional.first().map(|s| s.as_str()))
        };
        if key.is_empty() {
            bail!("usage: git config --get-all <key>");
        }
        let get_args = GetArgs {
            key,
            all: true,
            regexp: false,
            show_names: false,
            default: args.default_value.clone(),
            url: None,
            value: None,
            show_origin: false,
            show_scope: false,
        };
        return cmd_get(&args, &get_args, git_dir.as_deref(), value_pattern);
    }

    if let Some(ref pattern_raw) = args.get_regexp {
        let mut show_origin = args.show_origin;
        let (pattern, value_pattern) = if pattern_raw == "--show-origin" {
            show_origin = true;
            (
                args.positional.first().cloned().unwrap_or_default(),
                args.positional.get(1).map(|s| s.as_str()),
            )
        } else if pattern_raw.is_empty() {
            (
                args.positional.first().cloned().unwrap_or_default(),
                args.positional.get(1).map(|s| s.as_str()),
            )
        } else {
            (
                pattern_raw.clone(),
                args.positional.first().map(|s| s.as_str()),
            )
        };
        if pattern.is_empty() {
            bail!("usage: git config --get-regexp <pattern>");
        }
        let get_args = GetArgs {
            key: pattern,
            all: true,
            regexp: true,
            show_names: true,
            default: args.default_value.clone(),
            url: None,
            value: None,
            show_origin,
            show_scope: false,
        };
        return cmd_get(&args, &get_args, git_dir.as_deref(), value_pattern);
    }

    if let Some(ref key_raw) = args.get_urlmatch_key {
        reject_fixed_value(&args)?;
        let (key, url) = if key_raw.is_empty() {
            if args.positional.len() < 2 {
                bail!("usage: git config --get-urlmatch <key> <URL>");
            }
            (args.positional[0].as_str(), args.positional[1].as_str())
        } else {
            if args.positional.is_empty() {
                bail!("usage: git config --get-urlmatch <key> <URL>");
            }
            (key_raw.as_str(), args.positional[0].as_str())
        };
        return cmd_get_urlmatch(&args, key, url, git_dir.as_deref());
    }

    if let Some(ref key) = args.get_color_key {
        reject_fixed_value(&args)?;
        let default_color = args.positional.first().map(|s| s.as_str()).unwrap_or("");
        return cmd_get_color(key, default_color, git_dir.as_deref());
    }

    // Validate --default is only used with get operations
    if args.default_value.is_some() {
        let is_get_op = args.get_key.is_some()
            || args.get_all_key.is_some()
            || args.get_regexp.is_some()
            || args.get_urlmatch_key.is_some();
        if !is_get_op {
            let is_positional_get = args.positional.len() <= 1
                && args.unset_key.is_none()
                && args.unset_all_key.is_none()
                && args.add_key.is_none()
                && !args.remove_section
                && !args.rename_section
                && !args.list;
            if !is_positional_get {
                eprintln!("error: --default is only applicable to --get, --get-all, --get-regexp, and --get-urlmatch");
                std::process::exit(129);
            }
        }
    }

    if let Some(ref key_raw) = args.unset_key {
        let (key, value_pattern) = if key_raw.is_empty() {
            let key = args.positional.first().cloned().unwrap_or_default();
            let value_pattern = args.positional.get(1).map(|s| s.as_str());
            (key, value_pattern)
        } else {
            (key_raw.clone(), args.positional.first().map(|s| s.as_str()))
        };
        if key.is_empty() {
            bail!("usage: git config --unset <key>");
        }
        let unset_args = UnsetArgs {
            key,
            all: false,
            value: None,
        };
        return cmd_unset(&args, &unset_args, scope, &file_path, value_pattern, false);
    }

    if let Some(ref key_raw) = args.unset_all_key {
        let (key, value_pattern) = if key_raw.is_empty() {
            let key = args.positional.first().cloned().unwrap_or_default();
            let value_pattern = args.positional.get(1).map(|s| s.as_str());
            (key, value_pattern)
        } else {
            (key_raw.clone(), args.positional.first().map(|s| s.as_str()))
        };
        if key.is_empty() {
            bail!("usage: git config --unset-all <key>");
        }
        let unset_args = UnsetArgs {
            key,
            all: true,
            value: None,
        };
        return cmd_unset(&args, &unset_args, scope, &file_path, value_pattern, false);
    }

    if let Some(ref key_raw) = args.add_key {
        reject_fixed_value(&args)?;
        let (key, value) = if key_raw.is_empty() {
            if args.positional.len() < 2 {
                bail!("usage: git config --add <key> <value>");
            }
            (args.positional[0].clone(), args.positional[1].as_str())
        } else {
            if args.positional.is_empty() {
                bail!("missing value for --add");
            }
            (key_raw.clone(), args.positional[0].as_str())
        };
        return cmd_add(&args, &key, value, scope, &file_path);
    }

    if args.remove_section {
        reject_fixed_value(&args)?;
        if args.positional.is_empty() {
            bail!("missing section name");
        }
        return cmd_remove_section(scope, &file_path, &args.positional[0]);
    }

    if args.rename_section {
        reject_fixed_value(&args)?;
        if args.positional.len() < 2 {
            bail!("missing old-name and/or new-name");
        }
        return cmd_rename_section(scope, &file_path, &args.positional[0], &args.positional[1]);
    }

    // Legacy set: `git config key value`
    match args.positional.len() {
        0 => {
            // Git: `git config` with no operation is an error (t1300-config).
            bail!("no action specified");
        }
        1 => {
            if args.replace_all {
                bail!("error: wrong number of arguments, should be 2");
            }
            // Legacy get: `git config key`
            let get_args = GetArgs {
                key: args.positional[0].clone(),
                all: false,
                regexp: false,
                show_names: false,
                default: args.default_value.clone(),
                url: None,
                value: None,
                show_origin: false,
                show_scope: false,
            };
            cmd_get(&args, &get_args, git_dir.as_deref(), None)
        }
        2 => {
            if !args.global
                && !args.system
                && !args.worktree
                && args.file.is_none()
                && git_dir.is_none()
            {
                bail!("not in a git directory");
            }
            // Legacy set: `git config key value`
            // or `git config --replace-all key value`
            let set_args = SetArgs {
                key: args.positional[0].clone(),
                value: args.positional[1].clone(),
                all: args.replace_all,
                append: false,
            };
            cmd_set(&args, &set_args, scope, &file_path, None)
        }
        3 => {
            if args.replace_all {
                // `git config --replace-all key value value-pattern`
                let set_args = SetArgs {
                    key: args.positional[0].clone(),
                    value: args.positional[1].clone(),
                    all: true,
                    append: false,
                };
                cmd_set(
                    &args,
                    &set_args,
                    scope,
                    &file_path,
                    Some(&args.positional[2]),
                )
            } else {
                if !args.global
                    && !args.system
                    && !args.worktree
                    && args.file.is_none()
                    && git_dir.is_none()
                {
                    bail!("not in a git directory");
                }
                // `git config key value value-pattern` (legacy with value-pattern)
                let set_args = SetArgs {
                    key: args.positional[0].clone(),
                    value: args.positional[1].clone(),
                    all: false,
                    append: false,
                };
                cmd_set(
                    &args,
                    &set_args,
                    scope,
                    &file_path,
                    Some(&args.positional[2]),
                )
            }
        }
        _ => bail!("too many arguments"),
    }
}

// ── Subcommand implementations ──────────────────────────────────────

fn cmd_get(
    args: &Args,
    get_args: &GetArgs,
    git_dir: Option<&Path>,
    value_pattern: Option<&str>,
) -> Result<()> {
    require_fixed_value_pattern(args, value_pattern)?;
    let config = load_config(args, git_dir, ConfigReadIncludeMode::Lookup)?;
    let terminator = if args.null_terminated { '\0' } else { '\n' };
    let cwd = std::env::current_dir().ok();

    // Handle --url for URL matching (subcommand interface)
    if let Some(ref url) = get_args.url {
        if let Some(i) = get_args.key.find('.') {
            let (section, variable) = (&get_args.key[..i], &get_args.key[i + 1..]);
            let entries =
                grit_lib::config::get_urlmatch_entries(config.entries(), section, variable, url);
            let Some(entry) = entries.last() else {
                if let Some(ref default) = get_args.default {
                    let val = format_default_value(args, default)?;
                    print_default_value(args, &val, terminator);
                    return Ok(());
                }
                std::process::exit(1);
            };
            let val = entry.value.as_deref().unwrap_or("true");
            let val = format_typed_value(args, Some(&get_args.key), val)?;
            print!("{val}{terminator}");
        } else {
            let entries =
                grit_lib::config::get_urlmatch_all_in_section(config.entries(), &get_args.key, url);
            if entries.is_empty() {
                std::process::exit(1);
            }
            for (var_key, val, scope) in &entries {
                let val = format_typed_value(args, Some(var_key), val)?;
                let prefix = if get_args.show_scope || args.show_scope {
                    format!("{}	", scope)
                } else {
                    String::new()
                };
                print!("{prefix}{var_key} {val}{terminator}");
            }
        }
        return Ok(());
    }

    if get_args.regexp {
        let mut matches = config
            .get_regexp(&get_args.key)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        if let Some(pattern) = value_pattern {
            let mut filtered = Vec::new();
            for entry in matches {
                let value = entry.value.as_deref().unwrap_or("true");
                if value_matches_pattern(value, pattern, args.fixed_value)? {
                    filtered.push(entry);
                }
            }
            matches = filtered;
        }
        if matches.is_empty() {
            std::process::exit(1);
        }
        for entry in matches {
            let bare_boolean = entry.value.is_none();
            let want_bool_text = regexp_type_requests_bool_output(args);
            let prefix = config_entry_prefix_for_get(args, get_args, entry, cwd.as_deref());
            if args.name_only {
                print!("{}{}{}", prefix, entry.key, terminator);
            } else if get_args.show_names {
                // Bare keys are boolean true; Git prints only the key unless a bool type is requested
                // (t1300-config: get-regexp variable with no value vs get-regexp --bool).
                if bare_boolean && !want_bool_text {
                    print!("{}{}{}", prefix, entry.key, terminator);
                } else {
                    let val = entry.value.as_deref().unwrap_or("true");
                    let val = format_typed_value(args, Some(&entry.key), val)?;
                    if args.null_terminated {
                        print!("{}{}\n{}{}", prefix, entry.key, val, terminator);
                    } else {
                        print!("{}{} {}{}", prefix, entry.key, val, terminator);
                    }
                }
            } else {
                let val = entry.value.as_deref().unwrap_or("true");
                let val = format_typed_value(args, Some(&entry.key), val)?;
                print!("{}{}", val, terminator);
            }
        }
        return Ok(());
    }

    if get_args.all {
        let mut values = config.get_all(&get_args.key);
        if let Some(pattern) = value_pattern {
            filter_values_by_pattern(&mut values, pattern, args.fixed_value)?;
        }
        if values.is_empty() {
            if let Some(ref default) = get_args.default {
                let val = format_default_value(args, default)?;
                print_default_value(args, &val, terminator);
                return Ok(());
            }
            std::process::exit(1);
        }
        for val in values {
            let val = format_typed_value(args, Some(&get_args.key), &val)?;
            print!("{val}{terminator}");
        }
        return Ok(());
    }

    if let Some(pattern) = value_pattern {
        // --get with value-regex: get all values, filter, return last match
        let mut values = config.get_all(&get_args.key);
        filter_values_by_pattern(&mut values, pattern, args.fixed_value)?;
        if let Some(val) = values.last() {
            let val = format_typed_value(args, Some(&get_args.key), val)?;
            print!("{val}{terminator}");
            return Ok(());
        }
        if let Some(ref default) = get_args.default {
            let d = format_default_value(args, default)?;
            print_default_value(args, &d, terminator);
            return Ok(());
        }
        std::process::exit(1);
    }

    // For --path with :(optional) values, we need to check all values
    // and find the last non-optional-missing one.
    let has_path_type = type_is(args, ConfigValueType::Path);
    if has_path_type {
        if let Ok(canon) = grit_lib::config::canonical_key(&get_args.key) {
            if let Some(entry) = config
                .entries()
                .iter()
                .rev()
                .find(|entry| entry.key == canon)
            {
                if entry.value.is_none() {
                    let file = entry
                        .file
                        .as_deref()
                        .map(grit_lib::config::config_file_display_for_error)
                        .unwrap_or_else(|| "command line".to_owned());
                    return Err(fatal_config_parse(format!(
                        "fatal: bad config value for '{}' in file {file} at line {}",
                        get_args.key, entry.line
                    )));
                }
            }
        }
        let all_values = config.get_all(&get_args.key);
        // Find the last value that isn't optional-missing
        let last_valid = all_values
            .iter()
            .rev()
            .find(|v| !is_optional_missing_path(args, v));
        if let Some(val) = last_valid {
            let val = format_typed_value(args, Some(&get_args.key), val)?;
            print!("{val}{terminator}");
            return Ok(());
        }
        if let Some(ref default) = get_args.default {
            let val = format_default_value(args, default)?;
            print_default_value(args, &val, terminator);
            return Ok(());
        }
        std::process::exit(1);
    }

    let canon = grit_lib::config::canonical_key(&get_args.key).ok();
    let entry = canon.as_deref().and_then(|canon| {
        config
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.key == canon)
    });
    match entry.and_then(|entry| {
        entry
            .value
            .clone()
            .or_else(|| Some("true".to_owned()))
            .map(|v| (entry, v))
    }) {
        Some((entry, val)) => {
            let val = format_typed_value(args, Some(&get_args.key), &val)?;
            let prefix = config_entry_prefix_for_get(args, get_args, entry, cwd.as_deref());
            print!("{prefix}{val}{terminator}");
            Ok(())
        }
        None => {
            if let Some(ref default) = get_args.default {
                let val = format_default_value(args, default)?;
                if args.show_scope || get_args.show_scope {
                    print!("command	");
                }
                if args.show_origin || get_args.show_origin {
                    print!("command line:	");
                }
                print_default_value(args, &val, terminator);
                return Ok(());
            }
            std::process::exit(1);
        }
    }
}

fn cmd_set(
    args: &Args,
    set_args: &SetArgs,
    scope: ConfigScope,
    file_path: &Path,
    value_pattern: Option<&str>,
) -> Result<()> {
    if set_args.append {
        reject_fixed_value(args)?;
    } else {
        require_fixed_value_pattern(args, value_pattern)?;
    }
    reject_stdin_write(file_path)?;
    // Validate --comment: must not contain LF
    if let Some(ref c) = args.comment {
        if c.contains('\n') {
            bail!("invalid comment: must not contain newline");
        }
    }

    // Canonicalize the value if a type flag is given
    let value = canonicalize_value_for_set(args, &set_args.key, &set_args.value)?;
    let comment = args.comment.as_deref();

    // The harness sets `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME` and then runs
    // `git config --global init.defaultBranch <that value>` using grit. Real Git does not inject
    // that key into the global file from the env var alone, so `config --list` in t1300 would
    // wrongly include `init.defaultbranch=…` unless we skip this redundant write.
    if scope == ConfigScope::Global && !set_args.append && !set_args.all && value_pattern.is_none()
    {
        if let Ok(canon) = grit_lib::config::canonical_key(&set_args.key) {
            if canon == "init.defaultbranch" {
                if let Ok(test_branch) = std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME") {
                    if !test_branch.is_empty() && test_branch == value {
                        return Ok(());
                    }
                }
            }
        }
    }

    let mut config = match ConfigFile::from_path(file_path, scope).context("reading config file")? {
        Some(cfg) => cfg,
        None => ConfigFile::parse(file_path, "", scope)?,
    };

    let regex_value_pattern = regex_api_value_pattern(args, value_pattern);

    if set_args.append {
        config.add_value(&set_args.key, &value)?;
    } else if set_args.all {
        config.replace_all_with_comment(
            &set_args.key,
            &value,
            regex_value_pattern.as_deref(),
            comment,
        )?;
    } else if let Some(pattern) = value_pattern {
        let canon = grit_lib::config::canonical_key(&set_args.key)?;
        let mut matches: Vec<String> = config
            .entries
            .iter()
            .filter(|entry| entry.key == canon)
            .map(|entry| entry.value.clone().unwrap_or_else(|| "true".to_owned()))
            .collect();
        filter_values_by_pattern(&mut matches, pattern, args.fixed_value)?;
        if matches.len() > 1 {
            eprintln!("warning: {} has multiple values", set_args.key);
            std::process::exit(5);
        }
        config.replace_all_with_comment(
            &set_args.key,
            &value,
            regex_value_pattern.as_deref(),
            comment,
        )?;
    } else {
        config.set_with_comment(&set_args.key, &value, comment)?;
    }
    config.write().context("writing config file")?;
    Ok(())
}

fn cmd_unset(
    args: &Args,
    unset_args: &UnsetArgs,
    scope: ConfigScope,
    file_path: &Path,
    value_pattern: Option<&str>,
    preserve_empty_section_header_on_unset_all: bool,
) -> Result<()> {
    require_fixed_value_pattern(args, value_pattern)?;
    reject_stdin_write(file_path)?;
    let mut config = ConfigFile::from_path(file_path, scope).context("reading config file")?;
    let regex_value_pattern = regex_api_value_pattern(args, value_pattern);

    match config {
        Some(ref mut cfg) => {
            if unset_args.all {
                let preserve = preserve_empty_section_header_on_unset_all;
                let removed =
                    cfg.unset_matching(&unset_args.key, regex_value_pattern.as_deref(), preserve)?;
                if removed == 0 {
                    std::process::exit(5);
                }
            } else if value_pattern.is_some() {
                // --unset with value-pattern: remove only matching values
                let removed =
                    cfg.unset_matching(&unset_args.key, regex_value_pattern.as_deref(), false)?;
                if removed == 0 {
                    std::process::exit(5);
                }
            } else {
                // --unset (single): fail if multiple values exist
                let count = cfg.count(&unset_args.key)?;
                if count == 0 {
                    std::process::exit(5);
                }
                if count > 1 {
                    eprintln!("warning: {}: has multiple values", unset_args.key);
                    std::process::exit(5);
                }
                let removed = cfg.unset_matching(&unset_args.key, None, false)?;
                if removed == 0 {
                    std::process::exit(5);
                }
            }
            cfg.write().context("writing config file")?;
        }
        None => std::process::exit(5),
    }
    Ok(())
}

fn config_origin_prefix_with_separator(
    entry: &grit_lib::config::ConfigEntry,
    cwd: Option<&Path>,
    separator: char,
) -> String {
    if entry.scope == ConfigScope::Command {
        return format!("command line:{separator}");
    }
    let Some(file) = entry.file.as_deref() else {
        return if entry.scope == ConfigScope::Command {
            format!("command line:{separator}")
        } else {
            String::new()
        };
    };
    if file == Path::new("-") {
        return format!("standard input:{separator}");
    }
    if file.to_string_lossy().starts_with(':') {
        return format!("command line:{separator}");
    }
    let display_path = if entry.scope == ConfigScope::Global || file.is_relative() {
        file.display().to_string()
    } else if let Some(cwd) = cwd {
        let git_dir = cwd.join(".git");
        if file.starts_with(&git_dir) {
            file.strip_prefix(cwd)
                .map(|rel| rel.display().to_string())
                .unwrap_or_else(|_| file.display().to_string())
        } else {
            file.display().to_string()
        }
    } else {
        file.display().to_string()
    };
    format!("file:{}{}", quote_origin_path(&display_path), separator)
}

fn config_origin_prefix(entry: &grit_lib::config::ConfigEntry, cwd: Option<&Path>) -> String {
    config_origin_prefix_with_separator(entry, cwd, '\t')
}

fn quote_origin_path(path: &str) -> String {
    if path.contains('"') || path.contains(' ') || path.contains('\t') {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        path.to_owned()
    }
}

fn config_entry_prefix(
    args: &Args,
    entry: &grit_lib::config::ConfigEntry,
    cwd: Option<&Path>,
) -> String {
    let mut prefix = String::new();
    if args.show_scope {
        prefix.push_str(&format!("{}\t", entry.scope));
    }
    if args.show_origin {
        prefix.push_str(&config_origin_prefix(entry, cwd));
    }
    prefix
}

fn config_entry_prefix_for_list(
    args: &Args,
    entry: &grit_lib::config::ConfigEntry,
    cwd: Option<&Path>,
) -> String {
    let mut prefix = String::new();
    if args.show_scope {
        prefix.push_str(&format!("{}\t", entry.scope));
    }
    if args.show_origin {
        let separator = if args.null_terminated { '\0' } else { '\t' };
        prefix.push_str(&config_origin_prefix_with_separator(entry, cwd, separator));
    }
    prefix
}

fn config_entry_prefix_for_get(
    args: &Args,
    get_args: &GetArgs,
    entry: &grit_lib::config::ConfigEntry,
    cwd: Option<&Path>,
) -> String {
    let mut prefix = String::new();
    if args.show_scope || get_args.show_scope {
        prefix.push_str(&format!("{}	", entry.scope));
    }
    if args.show_origin || get_args.show_origin {
        prefix.push_str(&config_origin_prefix(entry, cwd));
    }
    prefix
}

fn cmd_list(args: &Args, git_dir: Option<&Path>) -> Result<()> {
    let config = load_config(args, git_dir, ConfigReadIncludeMode::List)?;
    let terminator = if args.null_terminated { '\0' } else { '\n' };
    let cwd = std::env::current_dir().ok();

    for entry in config.entries() {
        let prefix = config_entry_prefix_for_list(args, entry, cwd.as_deref());
        let raw_val = entry.value.as_deref().unwrap_or("true");
        let formatted = if has_type(args) {
            if is_optional_missing_path(args, raw_val) {
                continue;
            }
            match format_typed_value(args, Some(&entry.key), raw_val) {
                Ok(v) if type_is(args, ConfigValueType::Path) && v.is_empty() => {
                    continue;
                }
                Ok(v) => v,
                Err(_) => continue,
            }
        } else {
            raw_val.to_owned()
        };
        if args.name_only {
            print!("{}{}{}", prefix, entry.key, terminator);
        } else if args.null_terminated {
            if entry.value.is_some() || formatted != "true" {
                print!(
                    "{}{}
{}{}",
                    prefix, entry.key, formatted, terminator
                );
            } else {
                print!("{}{}{}", prefix, entry.key, terminator);
            }
        } else {
            print!("{}{}={}{}", prefix, entry.key, formatted, terminator);
        }
    }
    Ok(())
}

fn cmd_remove_section(scope: ConfigScope, file_path: &Path, name: &str) -> Result<()> {
    reject_stdin_write(file_path)?;
    let mut config = ConfigFile::from_path(file_path, scope).context("reading config file")?;

    match config {
        Some(ref mut cfg) => {
            if !cfg.remove_section(name)? {
                bail!("no such section: {name}");
            }
            cfg.write().context("writing config file")?;
        }
        None => bail!("config file not found: {}", file_path.display()),
    }
    Ok(())
}

fn cmd_rename_section(
    scope: ConfigScope,
    file_path: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<()> {
    reject_stdin_write(file_path)?;
    reject_overlong_config_lines(file_path)?;
    let mut config = ConfigFile::from_path(file_path, scope).context("reading config file")?;

    match config {
        Some(ref mut cfg) => {
            if !cfg.rename_section(old_name, new_name)? {
                bail!("no such section: {old_name}");
            }
            cfg.write().context("writing config file")?;
        }
        None => bail!("config file not found: {}", file_path.display()),
    }
    Ok(())
}

fn reject_overlong_config_lines(file_path: &Path) -> Result<()> {
    const MAX_CONFIG_LINE_LEN: usize = 512 * 1024;
    let Ok(content) = std::fs::read_to_string(file_path) else {
        return Ok(());
    };
    for (idx, line) in content.lines().enumerate() {
        if line.len() > MAX_CONFIG_LINE_LEN {
            return Err(anyhow::anyhow!(
                "refusing to work with overly long line in '{}' on line {}",
                file_path.display(),
                idx + 1
            ));
        }
    }
    Ok(())
}

fn cmd_add(
    _args: &Args,
    key: &str,
    value: &str,
    scope: ConfigScope,
    file_path: &Path,
) -> Result<()> {
    reject_stdin_write(file_path)?;
    let mut config = match ConfigFile::from_path(file_path, scope).context("reading config file")? {
        Some(cfg) => cfg,
        None => ConfigFile::parse(file_path, "", scope)?,
    };
    config.add_value(key, value)?;
    config.write().context("writing config file")?;
    Ok(())
}

fn cmd_edit(file_path: &Path) -> Result<()> {
    reject_stdin_write(file_path)?;
    // Resolve editor: GIT_EDITOR env → core.editor config → VISUAL env → EDITOR env → vi
    let git_dir = resolve_git_dir();
    let config = ConfigSet::load(git_dir.as_deref(), true).unwrap_or_default();

    let editor = std::env::var("GIT_EDITOR")
        .ok()
        .or_else(|| config.get("core.editor"))
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_owned());

    // Use shell to handle editors that include arguments/redirections
    // (matches Git's launch_editor behavior)
    let file_str = file_path.display().to_string();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$@\"", editor))
        .arg("--")
        .arg(&file_str)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;

    if !status.success() {
        bail!("editor exited with status {}", status);
    }
    Ok(())
}

fn reject_stdin_write(file_path: &Path) -> Result<()> {
    if file_path == Path::new("-") {
        return Err(fatal_config_parse(
            "fatal: writing to stdin is not supported",
        ));
    }
    Ok(())
}

/// Handle `--blob=<blob-ish>` — read config from a blob object (read-only).
/// Handle `--get-urlmatch <key> <URL>`.
fn cmd_get_urlmatch(args: &Args, key: &str, url: &str, git_dir: Option<&Path>) -> Result<()> {
    let config = load_config(args, git_dir, ConfigReadIncludeMode::Lookup)?;
    let terminator = if args.null_terminated { '\0' } else { '\n' };

    if let Some(dot) = key.find('.') {
        let section = &key[..dot];
        let variable = &key[dot + 1..];
        let entries =
            grit_lib::config::get_urlmatch_entries(config.entries(), section, variable, url);
        let Some(entry) = entries.last() else {
            std::process::exit(1);
        };
        let val = entry.value.as_deref().unwrap_or("true");
        let val = format_typed_value(args, Some(key), val)?;
        print!("{val}{terminator}");
    } else {
        // Section-only: return all variables from that section matching the URL
        let entries = grit_lib::config::get_urlmatch_all_in_section(config.entries(), key, url);
        if entries.is_empty() {
            std::process::exit(1);
        }
        for (var_key, val, scope) in &entries {
            let prefix = if args.show_scope {
                format!("{}	", scope)
            } else {
                String::new()
            };
            if val.is_empty()
                && !type_is(args, ConfigValueType::Bool)
                && !type_is(args, ConfigValueType::BoolOrInt)
                && !has_type(args)
            {
                print!("{prefix}{var_key}{terminator}");
            } else {
                let val = format_typed_value(args, Some(var_key), val)?;
                print!("{prefix}{var_key} {val}{terminator}");
            }
        }
    }
    Ok(())
}

/// Handle `--get-color <key> [<default>]`.
fn cmd_get_color(key: &str, default_color: &str, git_dir: Option<&Path>) -> Result<()> {
    let git_dir_resolved = git_dir.map(|p| p.to_path_buf());
    let config = ConfigSet::load(git_dir_resolved.as_deref(), true).unwrap_or_default();

    let color_str = if !key.is_empty() {
        config.get(key).unwrap_or_else(|| default_color.to_owned())
    } else {
        default_color.to_owned()
    };

    if color_str.is_empty() {
        return Ok(());
    }

    let ansi = parse_color(&color_str).map_err(|e| anyhow::anyhow!("{}", e))?;
    print!("{ansi}");
    Ok(())
}

fn cmd_blob(args: &Args, blob_spec: &str) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let oid = resolve_revision(&repo, blob_spec)
        .map_err(|_| anyhow::anyhow!("unable to resolve spec '{}' to a blob", blob_spec))?;
    let obj = repo
        .odb
        .read(&oid)
        .map_err(|_| anyhow::anyhow!("unable to read object {}", oid))?;
    if obj.kind != ObjectKind::Blob {
        bail!(
            "object {} is a {}, not a blob",
            oid,
            match obj.kind {
                ObjectKind::Tree => "tree",
                ObjectKind::Commit => "commit",
                ObjectKind::Tag => "tag",
                _ => "unknown",
            }
        );
    }
    let content = String::from_utf8(obj.data).context("blob is not valid UTF-8")?;
    let blob_path_str = blob_spec.to_string();
    let blob_path = std::path::Path::new(&blob_path_str);
    let config_file = ConfigFile::parse_with_origin(
        blob_path,
        &content,
        ConfigScope::Command,
        ConfigIncludeOrigin::Blob,
    )
    .with_context(|| format!("bad config in blob '{}'", blob_spec))?;
    let mut config = ConfigSet::new();
    let process_includes = !args.no_includes;
    if process_includes {
        let inc_ctx = IncludeContext {
            git_dir: Some(repo.git_dir.clone()),
            command_line_relative_include_is_error: false,
        };
        config
            .merge_file_with_includes(&config_file, true, &inc_ctx)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    } else {
        config.merge(&config_file);
    }

    let terminator = if args.null_terminated { '\0' } else { '\n' };

    // --list
    if args.list {
        for entry in config.entries() {
            let prefix = blob_config_prefix(args.show_scope, args.show_origin, blob_spec);
            let val = entry.value.as_deref().unwrap_or("true");
            if args.name_only {
                print!("{}{}{}", prefix, entry.key, terminator);
            } else {
                print!("{}{}={}{}", prefix, entry.key, val, terminator);
            }
        }
        return Ok(());
    }

    // --get-regexp
    if let Some(ref pattern) = args.get_regexp {
        let matches = config
            .get_regexp(pattern)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        if matches.is_empty() {
            std::process::exit(1);
        }
        for entry in matches {
            let bare_boolean = entry.value.is_none();
            let want_bool_text = regexp_type_requests_bool_output(args);
            if args.name_only || (bare_boolean && !want_bool_text) {
                print!("{}{}", entry.key, terminator);
            } else {
                let val = entry.value.as_deref().unwrap_or("true");
                let val = format_typed_value(args, Some(&entry.key), val)?;
                print!("{} {}{}", entry.key, val, terminator);
            }
        }
        return Ok(());
    }

    // --get
    if let Some(ref key) = args.get_key {
        match config.get(key) {
            Some(val) => {
                let val = format_typed_value(args, Some(key), &val)?;
                print!("{val}{terminator}");
                return Ok(());
            }
            None => std::process::exit(1),
        }
    }

    // --get-all
    if let Some(ref key) = args.get_all_key {
        let values = config.get_all(key);
        if values.is_empty() {
            std::process::exit(1);
        }
        for val in values {
            let val = format_typed_value(args, Some(key), &val)?;
            print!("{val}{terminator}");
        }
        return Ok(());
    }

    // Positional: `git config --blob=X key`
    if args.positional.len() == 1 {
        let lookup_key = &args.positional[0];
        match config.get(lookup_key) {
            Some(val) => {
                let val = format_typed_value(args, Some(lookup_key), &val)?;
                print!("{val}{terminator}");
                return Ok(());
            }
            None => std::process::exit(1),
        }
    }

    if args.positional.is_empty() && args.subcommand.is_none() {
        bail!("--blob requires a key or --list");
    }

    // Handle subcommands (get/list) with blob
    if let Some(ref sub) = args.subcommand {
        match sub {
            ConfigSubcommand::Get(get_args) => {
                if get_args.regexp {
                    let matches = config
                        .get_regexp(&get_args.key)
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    if matches.is_empty() {
                        std::process::exit(1);
                    }
                    for entry in matches {
                        let bare_boolean = entry.value.is_none();
                        let want_bool_text = regexp_type_requests_bool_output(args);
                        if get_args.show_names {
                            if bare_boolean && !want_bool_text {
                                print!("{}{}", entry.key, terminator);
                            } else {
                                let val = entry.value.as_deref().unwrap_or("true");
                                let val = format_typed_value(args, Some(&entry.key), val)?;
                                print!("{} {}{}", entry.key, val, terminator);
                            }
                        } else {
                            let val = entry.value.as_deref().unwrap_or("true");
                            let val = format_typed_value(args, Some(&entry.key), val)?;
                            print!("{}{}", val, terminator);
                        }
                    }
                    return Ok(());
                }
                if get_args.all {
                    let values = config.get_all(&get_args.key);
                    if values.is_empty() {
                        std::process::exit(1);
                    }
                    for val in values {
                        let val = format_typed_value(args, Some(&get_args.key), &val)?;
                        print!("{val}{terminator}");
                    }
                    return Ok(());
                }
                match config.get(&get_args.key) {
                    Some(val) => {
                        let val = format_typed_value(args, Some(&get_args.key), &val)?;
                        print!("{val}{terminator}");
                        Ok(())
                    }
                    None => std::process::exit(1),
                }
            }
            ConfigSubcommand::List(list_args) => {
                let show_scope = args.show_scope || list_args.show_scope;
                let show_origin = args.show_origin || list_args.show_origin;
                let name_only = args.name_only || list_args.name_only;
                for entry in config.entries() {
                    let prefix = blob_config_prefix(show_scope, show_origin, blob_spec);
                    let val = entry.value.as_deref().unwrap_or("true");
                    if name_only {
                        print!("{}{}{}", prefix, entry.key, terminator);
                    } else {
                        print!("{}{}={}{}", prefix, entry.key, val, terminator);
                    }
                }
                Ok(())
            }
            _ => bail!("--blob is read-only; cannot set/unset/edit"),
        }
    } else {
        bail!("--blob requires a key or --list");
    }
}

fn blob_config_prefix(show_scope: bool, show_origin: bool, blob_spec: &str) -> String {
    let mut prefix = String::new();
    if show_scope {
        prefix.push_str("command\t");
    }
    if show_origin {
        prefix.push_str(&format!("blob:{blob_spec}\t"));
    }
    prefix
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Wrap a message so the binary prints it verbatim and exits with code 128 (Git `die` convention).
fn fatal_config_parse(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(LibError::Message(msg.into()))
}

/// Filter a list of values by a value-pattern.
///
/// If `fixed_value` is true, the pattern is treated as a literal string.
/// Otherwise it is treated as a regex. A `!` prefix inverts the match.
fn filter_values_by_pattern(
    values: &mut Vec<String>,
    pattern: &str,
    fixed_value: bool,
) -> Result<()> {
    let mut filtered = Vec::new();
    for value in values.drain(..) {
        if value_matches_pattern(&value, pattern, fixed_value)? {
            filtered.push(value);
        }
    }
    *values = filtered;
    Ok(())
}

fn value_matches_pattern(value: &str, pattern: &str, fixed_value: bool) -> Result<bool> {
    if fixed_value {
        return Ok(value == pattern);
    }

    let (negated, pat) = if let Some(rest) = pattern.strip_prefix('!') {
        (true, rest)
    } else {
        (false, pattern)
    };
    let re =
        regex::Regex::new(pat).with_context(|| format!("invalid value-pattern regex: {pat}"))?;
    let matched = re.is_match(value);
    Ok(if negated { !matched } else { matched })
}

/// Resolve the git directory (best-effort; returns None outside a repo).
pub fn resolve_git_dir_pub() -> Option<PathBuf> {
    resolve_git_dir()
}

/// Directory to start repo discovery from.
///
/// When `PWD` is absolute and refers to the same location as [`std::env::current_dir`]
/// (same canonical path), prefer `PWD` so logical symlink components in the path string
/// match `gitdir:` patterns. If `PWD` points elsewhere (e.g. after `git -C`), use cwd.
fn discovery_start_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    if let Ok(pwd_s) = std::env::var("PWD") {
        let pwd = PathBuf::from(&pwd_s);
        if pwd.is_absolute() {
            if let (Ok(cwd_c), Ok(pwd_c)) = (cwd.canonicalize(), pwd.canonicalize()) {
                if cwd_c == pwd_c {
                    return Some(pwd);
                }
            }
        }
    }
    Some(cwd)
}

/// Resolve the git directory (best-effort; returns None outside a repo).
fn resolve_git_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GIT_DIR") {
        let p = PathBuf::from(dir);
        if p.is_absolute() {
            return Some(p);
        }
        if let Ok(cwd) = std::env::current_dir() {
            return Some(cwd.join(p));
        }
        return Some(p);
    }
    // Use library discovery so `GIT_CEILING_DIRECTORIES` (t1308 `nongit`) matches Git.
    grit_lib::repo::Repository::discover(None)
        .ok()
        .map(|r| r.git_dir)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(grit_lib::precompose_config::locate_git_dir_from_cwd)
        })
}

/// Target file for `git config --worktree` (matches Git `builtin/config.c`).
fn resolve_worktree_config_file(git_dir: &Path) -> Result<(ConfigScope, PathBuf)> {
    let common = common_git_dir_for_config(git_dir);
    if worktree_config_enabled(&common) {
        return Ok((ConfigScope::Worktree, git_dir.join("config.worktree")));
    }
    if registered_worktree_count(&common) > 1 {
        bail!(
            "--worktree cannot be used with multiple working trees unless the config\n\
extension worktreeConfig is enabled. Please read \"CONFIGURATION FILE\"\n\
section in \"git help worktree\" for details"
        );
    }
    Ok((ConfigScope::Local, common.join("config")))
}

/// Determine which config file to write to based on flags.
fn resolve_config_file(args: &Args, git_dir: Option<&Path>) -> Result<(ConfigScope, PathBuf)> {
    if let Some(ref path) = args.file {
        return Ok((ConfigScope::Local, path.clone()));
    }
    if args.system {
        let path = std::env::var("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/gitconfig"));
        return Ok((ConfigScope::System, path));
    }
    if args.global {
        let path = global_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine global config path"))?;
        return Ok((ConfigScope::Global, path));
    }
    if args.worktree {
        let gd = git_dir.ok_or_else(|| {
            fatal_config_parse("fatal: --worktree can only be used inside a git repository")
        })?;
        return resolve_worktree_config_file(gd);
    }
    if let Ok(path) = std::env::var("GIT_CONFIG") {
        if !path.is_empty() {
            return Ok((ConfigScope::Local, PathBuf::from(path)));
        }
    }
    // Default: local
    if let Some(gd) = git_dir {
        let common = common_git_dir_for_config(gd);
        Ok((ConfigScope::Local, common.join("config")))
    } else {
        // Outside repo, default to global for read operations
        let path = global_config_path().unwrap_or_else(|| PathBuf::from("/etc/gitconfig"));
        Ok((ConfigScope::Global, path))
    }
}

/// How includes are expanded for a read operation (matches Git split between `git config key` and `git config --list`).
#[derive(Clone, Copy)]
enum ConfigReadIncludeMode {
    /// Single-key lookups: expand includes for the default cascade and stdin; not for `-f path` unless `--includes`;
    /// not for `--global` / `--local` / `--system` unless `--includes`.
    Lookup,
    /// `--list` and similar: expand includes for scoped files too (still not for `-f path` unless `--includes`).
    List,
}

/// Load the config set, respecting file-scope flags.
fn load_config(
    args: &Args,
    git_dir: Option<&Path>,
    mode: ConfigReadIncludeMode,
) -> Result<ConfigSet> {
    let process_includes = match mode {
        ConfigReadIncludeMode::Lookup => {
            if let Some(ref p) = args.file {
                if p.to_string_lossy() == "-" {
                    !args.no_includes
                } else {
                    args.includes && !args.no_includes
                }
            } else if args.system || args.global || args.local {
                args.includes && !args.no_includes
            } else {
                !args.no_includes
            }
        }
        ConfigReadIncludeMode::List => {
            if let Some(ref p) = args.file {
                if p.to_string_lossy() == "-" {
                    !args.no_includes
                } else {
                    args.includes && !args.no_includes
                }
            } else if args.system || args.global || args.local || args.worktree {
                args.includes && !args.no_includes
            } else {
                !args.no_includes
            }
        }
    };
    let command_includes = !args.no_includes && args.file.is_none();
    let mut load_opts = LoadConfigOptions {
        include_system: true,
        process_includes,
        command_includes,
        include_ctx: IncludeContext {
            git_dir: git_dir.map(PathBuf::from),
            command_line_relative_include_is_error: true,
        },
    };

    // If a specific file is requested, only read that file
    if let Some(ref path) = args.file {
        let mut set = ConfigSet::new();
        let pseudo = path.to_string_lossy();
        let is_stdin = pseudo == "-";
        if !is_stdin && !path.exists() {
            if args.default_value.is_some() {
                return Ok(ConfigSet::new());
            }
            bail!(
                "fatal: unable to read config file '{}': No such file or directory",
                path.display()
            );
        }
        let file = if is_stdin {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Some(ConfigFile::parse_with_origin(
                path,
                &buf,
                ConfigScope::Local,
                ConfigIncludeOrigin::Stdin,
            )?)
        } else {
            ConfigFile::from_path(path, ConfigScope::Local)?
        };
        if let Some(f) = file {
            if process_includes {
                set.merge_file_with_includes(&f, true, &load_opts.include_ctx)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            } else {
                set.merge(&f);
            }
        }
        return Ok(set);
    }

    if args.system {
        let mut set = ConfigSet::new();
        let system_path = std::env::var("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/gitconfig"));
        let Some(f) = ConfigFile::from_path(&system_path, ConfigScope::System)? else {
            return Err(fatal_config_parse(format!(
                "fatal: unable to read config file '{}': No such file or directory",
                system_path.display()
            )));
        };
        if process_includes {
            set.merge_file_with_includes(&f, true, &load_opts.include_ctx)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        } else {
            set.merge(&f);
        }
        return Ok(set);
    }

    if args.global {
        let mut set = ConfigSet::new();
        if let Some(path) = global_config_path() {
            let Some(f) = ConfigFile::from_path(&path, ConfigScope::Global)? else {
                return Err(fatal_config_parse(format!(
                    "fatal: unable to read config file '{}': No such file or directory",
                    path.display()
                )));
            };
            if process_includes {
                set.merge_file_with_includes(&f, true, &load_opts.include_ctx)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            } else {
                set.merge(&f);
            }
        }
        return Ok(set);
    }

    if args.local {
        let gd = git_dir.ok_or_else(|| {
            fatal_config_parse("fatal: --local can only be used inside a git repository")
        })?;
        let mut set = ConfigSet::new();
        let common = common_git_dir_for_config(gd);
        if let Some(f) = ConfigFile::from_path(&common.join("config"), ConfigScope::Local)? {
            if process_includes {
                set.merge_file_with_includes(&f, true, &load_opts.include_ctx)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            } else {
                set.merge(&f);
            }
        }
        return Ok(set);
    }

    if args.worktree {
        let gd = git_dir.ok_or_else(|| {
            fatal_config_parse("fatal: --worktree can only be used inside a git repository")
        })?;
        let mut set = ConfigSet::new();
        let (scope, p) = resolve_worktree_config_file(gd)?;
        if let Some(f) = ConfigFile::from_path(&p, scope)? {
            set.merge(&f);
        }
        return Ok(set);
    }

    if let Ok(path) = std::env::var("GIT_CONFIG") {
        if !path.is_empty() {
            let path = PathBuf::from(path);
            let mut set = ConfigSet::new();
            if let Some(f) = ConfigFile::from_path(&path, ConfigScope::Local)? {
                if process_includes {
                    set.merge_file_with_includes(&f, true, &load_opts.include_ctx)
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                } else {
                    set.merge(&f);
                }
            }
            return Ok(set);
        }
    }

    // Default: full cascade
    load_opts.include_system = true;
    ConfigSet::load_with_options(git_dir, &load_opts).map_err(|e| anyhow::anyhow!("{}", e))
}

/// Get the path for the global config file.
fn global_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GIT_CONFIG_GLOBAL") {
        return Some(PathBuf::from(p));
    }
    let home_config = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".gitconfig"));
    // If ~/.gitconfig exists, use it
    if let Some(ref p) = home_config {
        if p.exists() {
            return home_config;
        }
    }
    // Fall back to XDG config
    let xdg_config = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        Some(PathBuf::from(xdg).join("git/config"))
    } else {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".config/git/config"))
    };
    if let Some(ref p) = xdg_config {
        if p.exists() {
            return xdg_config;
        }
    }
    // Return ~/.gitconfig as the default path for writing
    home_config
}

/// Returns whether `--default` is valid for the selected operation.
fn default_supported(args: &Args) -> bool {
    if matches!(args.subcommand, Some(ConfigSubcommand::Get(_))) {
        return true;
    }

    args.get_key.is_some()
        || args.get_all_key.is_some()
        || args.get_regexp.is_some()
        || args.positional.len() == 1
}

/// Formats a default value and adds Git-compatible context on failure.
fn format_default_value(args: &Args, val: &str) -> Result<String> {
    format_typed_value(args, None, val).map_err(|err| {
        if type_is(args, ConfigValueType::Int) {
            fatal_config_parse(format!("fatal: bad numeric config value '{val}'"))
        } else {
            err.context("failed to format default config value")
        }
    })
}

fn print_default_value(args: &Args, val: &str, terminator: char) {
    if type_is(args, ConfigValueType::Color) {
        print!("{val}");
    } else {
        print!("{val}{terminator}");
    }
}

/// Canonicalize a value for writing based on type flags.
///
/// When `--bool` is used, the value is validated and written as "true"/"false".
/// When `--int` is used, the value is validated and written as a plain integer.
/// When `--bool-or-int` is used, booleans are stored as "true"/"false" and
/// integers as plain numbers.
fn is_pack_allow_pack_reuse_key(config_key: &str) -> bool {
    grit_lib::config::canonical_key(config_key).ok().as_deref() == Some("pack.allowpackreuse")
}

fn canonicalize_value_for_set(args: &Args, config_key: &str, val: &str) -> Result<String> {
    if is_pack_allow_pack_reuse_key(config_key) {
        let t = val.trim();
        let lower = t.to_ascii_lowercase();
        if lower == "single" || lower == "multi" {
            return Ok(lower);
        }
        match parse_bool(t) {
            Ok(b) => return Ok(if b { "true".into() } else { "false".into() }),
            Err(_) => {
                return Err(fatal_config_parse(format!(
                    "fatal: invalid pack.allowPackReuse value: '{val}' for '{config_key}'"
                )));
            }
        }
    }

    if !is_pack_allow_pack_reuse_key(config_key) && type_is(args, ConfigValueType::Bool) {
        match parse_bool(val) {
            Ok(b) => return Ok(if b { "true" } else { "false" }.to_owned()),
            Err(_) => {
                return Err(fatal_config_parse(format!(
                    "fatal: bad boolean config value '{val}' for '{config_key}'"
                )));
            }
        }
    }

    if type_is(args, ConfigValueType::Int) {
        match parse_i64(val) {
            Ok(n) => return Ok(n.to_string()),
            Err(_) => {
                return Err(fatal_config_parse(format!(
                    "fatal: bad numeric config value '{val}' for '{config_key}'"
                )));
            }
        }
    }

    if type_is(args, ConfigValueType::BoolOrInt) {
        // Try named booleans first (not numbers — those go to int)
        match val.to_lowercase().as_str() {
            "true" | "yes" | "on" => return Ok("true".to_owned()),
            "false" | "no" | "off" => return Ok("false".to_owned()),
            _ => {}
        }
        // Then as integer
        if let Ok(n) = parse_i64(val) {
            return Ok(n.to_string());
        }
        return Err(fatal_config_parse(format!(
            "fatal: bad bool-or-int config value '{val}' for '{config_key}'"
        )));
    }

    if type_is(args, ConfigValueType::Color) {
        match parse_color(val) {
            Ok(_) => return Ok(val.to_owned()),
            Err(e) => bail!("cannot parse color: {}", e),
        }
    }

    Ok(val.to_owned())
}

/// Check if a value with --path type is an optional path that doesn't exist.
/// Returns true if the value should be skipped.
fn is_optional_missing_path(args: &Args, val: &str) -> bool {
    if type_is(args, ConfigValueType::Path) && val.starts_with(":(optional)") {
        return grit_lib::config::parse_path_optional(val).is_none();
    }
    false
}

fn format_typed_value(args: &Args, config_key: Option<&str>, val: &str) -> Result<String> {
    if let Some(key) = config_key {
        if is_pack_allow_pack_reuse_key(key) {
            return Ok(val.trim().to_string());
        }
    }

    if type_is(args, ConfigValueType::Bool) {
        match parse_bool(val) {
            Ok(b) => {
                return Ok(if b {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                })
            }
            Err(err) => {
                if let Some(key) = config_key {
                    return Err(fatal_config_parse(format!(
                        "fatal: bad boolean config value '{val}' for '{key}'"
                    )));
                }
                bail!("{}", err);
            }
        }
    }

    if type_is(args, ConfigValueType::Int) {
        match parse_i64(val) {
            Ok(n) => return Ok(n.to_string()),
            Err(err) => {
                if let Some(key) = config_key {
                    return Err(fatal_config_parse(format!(
                        "fatal: bad numeric config value '{val}' for '{key}' in file .git/config: invalid unit"
                    )));
                }
                bail!("{}", err);
            }
        }
    }

    if type_is(args, ConfigValueType::Path) {
        if val.starts_with("~/") && std::env::var_os("HOME").is_none() {
            return Err(fatal_config_parse(format!(
                "fatal: failed to expand user dir in: {val}"
            )));
        }
        return match grit_lib::config::parse_path_optional(val) {
            Some(p) => Ok(p),
            None => Ok(String::new()), // optional path missing — caller should check is_optional_missing_path
        };
    }

    if type_is(args, ConfigValueType::BoolOrInt) {
        // Try as named bool first
        match val.to_lowercase().as_str() {
            "true" | "yes" | "on" => return Ok("true".to_owned()),
            "" => return Ok("false".to_owned()),
            "false" | "no" | "off" => return Ok("false".to_owned()),
            _ => {}
        }
        // Then as integer
        match parse_i64(val) {
            Ok(n) => return Ok(n.to_string()),
            Err(err) => {
                if let Some(key) = config_key {
                    return Err(fatal_config_parse(format!(
                        "fatal: bad bool-or-int config value '{val}' for '{key}'"
                    )));
                }
                bail!("{}", err);
            }
        }
    }

    if type_is(args, ConfigValueType::Color) {
        match parse_color(val) {
            Ok(ansi) => return Ok(ansi),
            Err(e) => bail!("{}", e),
        }
    }

    if type_is(args, ConfigValueType::ExpiryDate) {
        return format_expiry_date(val);
    }

    Ok(val.to_owned())
}

/// Formats an expiry-date value as an epoch timestamp.
fn format_expiry_date(val: &str) -> Result<String> {
    let trimmed = val.trim();

    if trimmed.eq_ignore_ascii_case("never") {
        return Ok("0".to_owned());
    }

    if let Ok(n) = parse_i64(trimmed) {
        return Ok(n.to_string());
    }

    if let Ok((ts, _)) = grit_lib::git_date::parse::parse_date_basic(trimmed) {
        return Ok(ts.to_string());
    }

    let mut err = 0;
    let ts = grit_lib::git_date::approx::approxidate_careful(trimmed, Some(&mut err));
    if err == 0 {
        return Ok(ts.to_string());
    }

    bail!("invalid expiry date '{}'", val);
}

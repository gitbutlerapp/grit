//! Top-level `-C`, `-c`, `--git-dir` and `--work-tree`: pick the repository and override
//! config for one invocation. Accepted only before the subcommand, like Git.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::builder::TypedValueParser as _;
use clap::Args;
use grit_lib::config::environment::ConfigOverride;

use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};

/// Repository selection and config overrides applied before any command runs.
#[derive(Debug, Args, Default)]
pub struct GlobalRepoArgs {
    /// Run as if grit was started in DIR (repeatable; each one is relative to the previous).
    #[arg(
        short = 'C',
        value_name = "DIR",
        value_parser = clap::builder::OsStringValueParser::new().map(PathBuf::from)
    )]
    pub dirs: Vec<PathBuf>,
    /// Set a config value for this invocation only (repeatable; a bare KEY means true).
    #[arg(
        short = 'c',
        value_name = "KEY=VALUE",
        value_parser = parse_config_override
    )]
    pub config: Vec<ConfigOverride>,
    /// Use this repository directory instead of discovering one (same as `GIT_DIR`).
    #[arg(long = "git-dir", value_name = "PATH")]
    pub git_dir: Option<PathBuf>,
    /// Use this directory as the work tree root (same as `GIT_WORK_TREE`).
    #[arg(long = "work-tree", value_name = "PATH")]
    pub work_tree: Option<PathBuf>,
    /// Skip opportunistic index updates that take optional locks (same as `GIT_OPTIONAL_LOCKS=0`).
    #[arg(long = "no-optional-locks", global = true)]
    pub no_optional_locks: bool,
}

fn parse_config_override(spec: &str) -> Result<ConfigOverride, String> {
    spec.parse()
        .map_err(|err: grit_lib::error::Error| err.to_string())
}

impl GlobalRepoArgs {
    /// Change directory for each `-C`, then export the resulting `GIT_DIR`, `GIT_WORK_TREE`
    /// and `GIT_CONFIG_PARAMETERS`, so every [`crate::context::environment`] snapshot, hook and
    /// spawned `grit` process sees the same repository and config.
    ///
    /// Call once at startup, before any thread is spawned.
    /// Returns a structured usage error if the final cwd or repository globals
    /// cannot be represented by the library's UTF-8 discovery environment.
    pub fn apply(&self) -> Result<()> {
        let mut changed_dir = false;
        for dir in self.dirs.iter().filter(|d| !d.as_os_str().is_empty()) {
            std::env::set_current_dir(dir)
                .with_context(|| format!("cannot change to '{}'", dir.display()))?;
            changed_dir = true;
        }
        let cwd = std::env::current_dir().context("cannot resolve current working directory")?;
        // Validate after all -C traversal, but before repository discovery or the
        // environment builders can replace invalid bytes with U+FFFD.
        for (label, path) in [
            ("working directory", Some(cwd.as_path())),
            ("--git-dir path", self.git_dir.as_deref()),
            ("--work-tree path", self.work_tree.as_deref()),
        ] {
            if path.is_some_and(|path| path.to_str().is_none()) {
                let message = format!("{label} must be valid UTF-8 for repository selection");
                return Err(StructuredCliError {
                    payload: StructuredJsonError::new(message.clone(), ErrorKind::Usage),
                    human: message,
                }
                .into());
            }
        }
        if changed_dir {
            std::env::set_var("PWD", cwd);
        }
        if self.no_optional_locks {
            std::env::set_var("GIT_OPTIONAL_LOCKS", "0");
        }
        if self.git_dir.is_none() && self.work_tree.is_none() && self.config.is_empty() {
            return Ok(());
        }
        let mut env = crate::context::environment();
        if let Some(git_dir) = &self.git_dir {
            env = env.with_git_dir(git_dir);
        }
        if let Some(work_tree) = &self.work_tree {
            env = env.with_work_tree(work_tree);
        }
        for config_override in &self.config {
            env.push_config_override(config_override);
        }
        for (key, value) in [
            ("GIT_DIR", env.git_dir),
            ("GIT_WORK_TREE", env.git_work_tree),
            ("GIT_CONFIG_PARAMETERS", env.git_config_parameters),
        ] {
            if let Some(value) = value {
                std::env::set_var(key, OsString::from(value));
            }
        }
        Ok(())
    }
}

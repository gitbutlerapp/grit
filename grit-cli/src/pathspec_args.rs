//! Shared `--pathspec-from-file` / trailing path arguments for porcelain commands.

use anyhow::{bail, Context, Result};
use clap::Args;
use grit_lib::pathspec::{PathspecInput, PathspecResolveError, ResolvedPathspecs};
use grit_lib::repo::Repository;
use std::io::Read;
/// Path arguments plus Git-compatible `--pathspec-from-file` / `--pathspec-file-nul`.
#[derive(Debug, Clone, Default, Args)]
pub struct PathspecArgs {
    /// Files or directories to limit the command to.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
    /// Read pathspecs from a file (`-` for stdin) instead of or in addition to argv paths.
    #[arg(long = "pathspec-from-file", value_name = "FILE|-")]
    pub pathspec_from_file: Option<String>,
    /// `--pathspec-from-file` entries are NUL-separated.
    #[arg(long = "pathspec-file-nul", requires = "pathspec_from_file")]
    pub pathspec_file_nul: bool,
}

/// Like [`PathspecArgs`], but paths are trailing (after other positionals).
#[derive(Debug, Clone, Default, Args)]
pub struct TrailingPathspecArgs {
    /// Limit the command to these paths (after `--`).
    #[arg(value_name = "PATH", last = true, allow_hyphen_values = true)]
    pub paths: Vec<String>,
    /// Read pathspecs from a file (`-` for stdin) instead of or in addition to argv paths.
    #[arg(long = "pathspec-from-file", value_name = "FILE|-")]
    pub pathspec_from_file: Option<String>,
    /// `--pathspec-from-file` entries are NUL-separated.
    #[arg(long = "pathspec-file-nul", requires = "pathspec_from_file")]
    pub pathspec_file_nul: bool,
}

impl PathspecArgs {
    /// Resolve argv and optional from-file pathspecs against `repo` and the process cwd.
    pub fn resolve(&self, repo: &Repository) -> Result<ResolvedPathspecs> {
        resolve_pathspec_args(
            repo,
            &self.paths,
            self.pathspec_from_file.as_deref(),
            self.pathspec_file_nul,
        )
    }

    /// Require at least one pathspec source (argv or from-file).
    pub fn require_non_empty(&self) -> Result<()> {
        if self.paths.is_empty() && self.pathspec_from_file.is_none() {
            bail!("you must specify path(s)");
        }
        Ok(())
    }
}

impl TrailingPathspecArgs {
    /// Resolve argv and optional from-file pathspecs against `repo` and the process cwd.
    pub fn resolve(&self, repo: &Repository) -> Result<ResolvedPathspecs> {
        resolve_pathspec_args(
            repo,
            &self.paths,
            self.pathspec_from_file.as_deref(),
            self.pathspec_file_nul,
        )
    }

    /// Require at least one pathspec source (argv or from-file).
    pub fn require_non_empty(&self) -> Result<()> {
        if self.paths.is_empty() && self.pathspec_from_file.is_none() {
            bail!("you must specify path(s)");
        }
        Ok(())
    }
}

fn resolve_pathspec_args(
    repo: &Repository,
    paths: &[String],
    pathspec_from_file: Option<&str>,
    pathspec_file_nul: bool,
) -> Result<ResolvedPathspecs> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let from_file = match pathspec_from_file {
        None => None,
        Some(path) => {
            let data = read_pathspec_source(path)?;
            Some(PathspecInput::new(data, pathspec_file_nul))
        }
    };
    ResolvedPathspecs::resolve(repo, &cwd, paths, from_file).map_err(map_pathspec_resolve)
}

fn read_pathspec_source(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("could not read pathspecs from stdin")?;
        return Ok(buf);
    }
    std::fs::read(path).with_context(|| format!("could not read '{path}'"))
}

fn map_pathspec_resolve(err: PathspecResolveError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

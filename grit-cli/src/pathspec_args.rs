//! Shared `--pathspec-from-file` / trailing path arguments for porcelain commands.

use anyhow::{bail, Context, Result};
use clap::Args;
use grit_lib::error::PathspecFromFileParseError;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{
    parse_pathspecs_from_source, PathspecInput, PathspecResolveError, ResolvedPathspecs,
};
use std::io::Read;
use std::sync::OnceLock;

static PATHSPEC_STDIN_BYTES: OnceLock<Vec<u8>> = OnceLock::new();
/// Path arguments plus Git-compatible `--pathspec-from-file` / `--pathspec-file-nul`.
#[derive(Debug, Clone, Default, Args)]
pub struct PathspecArgs {
    /// Files or directories to limit the command to.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
    /// Read pathspecs from a file (`-` for stdin) instead of or in addition to argv paths.
    #[arg(
        long = "pathspec-from-file",
        value_name = "FILE|-",
        allow_hyphen_values = true
    )]
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
    #[arg(
        long = "pathspec-from-file",
        value_name = "FILE|-",
        allow_hyphen_values = true
    )]
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
    /// True when argv or `--pathspec-from-file` supplies at least one pathspec.
    #[must_use]
    pub fn has_any(&self) -> bool {
        !self.paths.is_empty() || self.pathspec_from_file.is_some()
    }

    /// Unresolved pathspec tokens: `--pathspec-from-file` entries first, then argv paths.
    ///
    /// `extra_argv_paths` supplies disambiguated path operands (for example diff revision/path
    /// parsing). When non-empty, they replace trailing `paths` so operands are not duplicated.
    pub fn pathspec_source_tokens(&self, extra_argv_paths: &[String]) -> Result<Vec<String>> {
        let mut sources = self.pathspec_file_entries()?.unwrap_or_default();
        if !extra_argv_paths.is_empty() {
            sources.extend(extra_argv_paths.iter().cloned());
        } else {
            sources.extend(self.paths.iter().cloned());
        }
        if self.has_any() {
            if sources.is_empty() {
                return Err(map_pathspec_resolve(PathspecResolveError::EmptyPathspec));
            }
            for sel in &sources {
                if sel.is_empty() {
                    return Err(map_pathspec_resolve(PathspecResolveError::EmptyPathspec));
                }
            }
        }
        Ok(sources)
    }

    /// Entries of `--pathspec-from-file`, or `None` when it was not given.
    pub fn pathspec_file_entries(&self) -> Result<Option<Vec<String>>> {
        let Some(path) = self.pathspec_from_file.as_deref() else {
            return Ok(None);
        };
        let data = read_pathspec_source(path)?;
        parse_pathspecs_from_source(&data, self.pathspec_file_nul)
            .map(Some)
            .map_err(map_pathspec_from_file_parse)
    }

    /// Pathspec tokens for rev walks (`RevWalkListOptions::paths`): argv plus file lines.
    ///
    /// Returns unresolved tokens; [`Repository::resolve_rev_walk_pathspecs`] runs once when the
    /// walk is built in `grit-lib`.
    pub fn rev_walk_tokens(&self, _repo: &Repository) -> Result<Vec<String>> {
        if !self.has_any() {
            return Ok(Vec::new());
        }
        self.pathspec_source_tokens(&[])
    }

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
        if let Some(cached) = PATHSPEC_STDIN_BYTES.get() {
            return Ok(cached.clone());
        }
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("could not read pathspecs from stdin")?;
        let _ = PATHSPEC_STDIN_BYTES.set(buf.clone());
        return Ok(buf);
    }
    std::fs::read(path).with_context(|| format!("could not read '{path}'"))
}

fn map_pathspec_from_file_parse(err: PathspecFromFileParseError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

fn map_pathspec_resolve(err: PathspecResolveError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

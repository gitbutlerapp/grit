//! Git hooks scoped to a [`Repository`](crate::repo::Repository).

use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::hooks::resolve_hooks_dir;
use crate::repo::Repository;

/// Where a traditional hook script was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSource {
    /// Default `.git/hooks` (or worktree commondir hooks).
    HooksDir,
    /// `core.hooksPath` from repository config.
    CoreHooksPath,
}

/// One hook script on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInfo {
    /// Basename of the hook (e.g. `pre-commit`).
    pub name: String,
    /// Absolute path to the hook file.
    pub path: PathBuf,
    /// Whether the file has an executable bit (always `true` on Windows).
    pub executable: bool,
    /// Whether the script lives under the default hooks dir or `core.hooksPath`.
    pub source: HookSource,
}

/// Hook discovery for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryHooks<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryHooks<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// List traditional hook scripts in the resolved hooks directory.
    ///
    /// Only regular files directly under the hooks directory are returned (no
    /// `hook.<name>.*` config aliases — those are resolved at run time).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Config`] when config cannot be read, or I/O errors from
    /// reading the hooks directory.
    pub fn list(&self) -> Result<Vec<HookInfo>> {
        let config = self.repo.config()?;
        list_hooks_in_dir(self.repo, config.as_ref())
    }
}

fn list_hooks_in_dir(repo: &Repository, config: &ConfigSet) -> Result<Vec<HookInfo>> {
    let hooks_dir = resolve_hooks_dir(repo);
    let source = if config.get("core.hooksPath").is_some() {
        HookSource::CoreHooksPath
    } else {
        HookSource::HooksDir
    };

    let read_dir = match fs::read_dir(&hooks_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut hooks = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let executable = hook_is_executable(&path);
        hooks.push(HookInfo {
            name,
            path,
            executable,
            source,
        });
    }
    hooks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hooks)
}

fn hook_is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

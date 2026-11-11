//! Git hooks scoped to a [`Repository`](crate::repo::Repository).

#![allow(deprecated)]

use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::hooks::{
    resolve_hooks_dir, run_commit_hook, run_commit_hook_checked, run_hook, run_hook_in_git_dir,
    run_post_checkout, run_post_merge, run_post_rewrite, run_pre_push_hook, run_pre_rebase,
    CommitHookEnv, HookOutput, HookResult, PrePushRef, RewriteKind,
};
use crate::list_limit::Limited;
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Where a traditional hook script was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookSource {
    /// Default `.git/hooks` (or worktree commondir hooks).
    HooksDir,
    /// `core.hooksPath` from repository config.
    CoreHooksPath,
}

/// One hook script on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// Hook discovery and dispatch for one open repository.
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

    /// Resolved hooks directory for this repository (honours `core.hooksPath`).
    #[must_use]
    pub fn hooks_dir(&self) -> PathBuf {
        resolve_hooks_dir(self.repo)
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
    pub fn list(&self, limit: Option<usize>) -> Result<Limited<HookInfo>> {
        let config = self.repo.config().snapshot()?;
        let hooks = list_hooks_in_dir(self.repo, config.as_ref())?;
        Ok(Limited::from_vec(hooks, limit))
    }

    /// Run a hook by name with the given arguments (Git-compatible multihooks).
    ///
    /// # Errors
    ///
    /// Propagates [`crate::hooks::HookError`] and repository configuration failures.
    pub fn run(
        &self,
        hook_name: &str,
        args: &[&str],
        stdin_data: Option<&[u8]>,
    ) -> Result<HookResult> {
        run_hook(self.repo, hook_name, args, stdin_data)
    }

    /// Run commit-style hooks with index/prefix environment (Git `run_commit_hook`).
    ///
    /// # Errors
    ///
    /// Propagates hook and configuration failures.
    pub fn run_commit(
        &self,
        hook_name: &str,
        args: &[&str],
        stdin_data: Option<&[u8]>,
        commit_env: &CommitHookEnv<'_>,
    ) -> Result<HookResult> {
        run_commit_hook(self.repo, hook_name, args, stdin_data, commit_env)
    }

    /// Like [`Self::run_commit`], but a non-zero hook exit is an error.
    pub fn run_commit_checked(
        &self,
        hook_name: &str,
        args: &[&str],
        stdin_data: Option<&[u8]>,
        commit_env: &CommitHookEnv<'_>,
    ) -> Result<()> {
        run_commit_hook_checked(self.repo, hook_name, args, stdin_data, commit_env)
    }

    /// Run a server-side hook with cwd = `GIT_DIR` (receive-pack protocol).
    #[must_use]
    pub fn run_in_git_dir(
        &self,
        hook_name: &str,
        args: &[&str],
        stdin_data: Option<&[u8]>,
        env_vars: &[(&str, &str)],
    ) -> HookResult {
        run_hook_in_git_dir(self.repo, hook_name, args, stdin_data, env_vars)
    }

    /// Run `post-checkout` after `HEAD` moved.
    #[must_use]
    pub fn run_post_checkout(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        branch_checkout: bool,
    ) -> bool {
        run_post_checkout(self.repo, old, new, branch_checkout)
    }

    /// Run `post-merge` after a merge or fast-forward updated the work tree.
    #[must_use]
    pub fn run_post_merge(&self, squash: bool) -> bool {
        run_post_merge(self.repo, squash)
    }

    /// Run `post-rewrite` with rewritten commit pairs on stdin.
    #[must_use]
    pub fn run_post_rewrite(&self, kind: RewriteKind, pairs: &[(ObjectId, ObjectId)]) -> bool {
        run_post_rewrite(self.repo, kind, pairs)
    }

    /// Run `pre-rebase` before a rebase rewrites commits.
    pub fn run_pre_rebase(&self, upstream: Option<&str>) -> Result<bool> {
        run_pre_rebase(self.repo, upstream)
    }

    /// Run the client-side `pre-push` hook before sending refs.
    pub fn run_pre_push(&self, remote: &str, url: &str, refs: &[PrePushRef]) -> Result<HookOutput> {
        run_pre_push_hook(self.repo, remote, url, refs)
    }
}

fn list_hooks_in_dir(repo: &Repository, config: &ConfigSet) -> Result<Vec<HookInfo>> {
    let hooks_dir = resolve_hooks_dir(repo);
    let source = if config.value("core.hooksPath").is_some() {
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

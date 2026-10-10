//! Clone a remote repository: initialize, configure `origin`, fetch, and set up the default branch.
//!
//! File checkout is left to the caller (typically [`crate::porcelain::checkout`]).

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::fetch::{fetch_operation_identity, Progress};
use crate::objects::ObjectId;
use crate::ref_storage::RefStorageFormat;
use crate::refs;
use crate::remote::{HttpClientFactory, Remote, DEFAULT_REMOTE};
use crate::repo::{init_repository, Repository};
use crate::transfer::{CloneReflog, FetchOptions, TagMode};
use crate::transport_path::{
    absolute_local_clone_source_url, should_store_absolute_local_clone_url,
};

/// Errors during [`clone`].
#[derive(Debug, Error)]
pub enum CloneError {
    /// Destination exists and is not empty.
    #[error("destination '{0}' already exists and is not empty")]
    DestNotEmpty(String),
    /// Underlying library failure.
    #[error(transparent)]
    Library(#[from] Error),
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] crate::remote::RemoteError),
}

/// Options for [`clone`].
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Source URL or path (as passed on the command line).
    pub url: String,
    /// Directory to create or use for the new repository.
    pub dest: PathBuf,
    /// Remote name to configure (default [`DEFAULT_REMOTE`]).
    pub remote_name: String,
    /// Process environment for config and reflog identity.
    pub environment: Environment,
    /// Initial branch name used when initializing the empty repo (`main`).
    pub initial_branch: String,
}

impl CloneOptions {
    /// Options for cloning into `dest` from `url`, using the current process environment.
    #[must_use]
    pub fn new(url: impl Into<String>, dest: PathBuf) -> Self {
        Self {
            url: url.into(),
            dest,
            remote_name: DEFAULT_REMOTE.to_owned(),
            environment: Environment::capture_process(), // hygiene: CLI boundary — snapshot process environment for clone
            initial_branch: "main".to_owned(),
        }
    }
}

/// Result of a successful [`clone`] before working-tree checkout.
#[derive(Debug)]
pub struct CloneOutcome {
    /// Source URL string from the options.
    pub url: String,
    /// Destination path.
    pub path: PathBuf,
    /// Short name of the default branch that was created.
    pub branch: String,
    /// Commit to check out on that branch.
    pub checkout_oid: ObjectId,
    /// Open handle to the new repository.
    pub repo: Repository,
}

/// Derive a destination directory from a clone URL (last path component, minus `.git`).
#[must_use]
pub fn derive_clone_dir(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

/// Whether `path` is an existing empty directory (used for failed-clone cleanup policy).
#[must_use]
pub fn dest_preexisted_empty(path: &Path) -> bool {
    path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
}

/// Remove a partially created clone destination.
///
/// When `keep_toplevel` is true, only contents of `path` are removed (empty dir existed before clone).
pub fn cleanup_failed_clone(path: &Path, keep_toplevel: bool) {
    if keep_toplevel {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(p);
            } else {
                let _ = fs::remove_file(p);
            }
        }
    } else {
        let _ = fs::remove_dir_all(path);
    }
}

/// Initialize, fetch, and configure the default branch; does not check out files.
///
/// # Errors
///
/// Returns [`CloneError`] when the destination is invalid, fetch fails, or no branch exists.
pub fn clone(
    opts: &CloneOptions,
    progress: &mut dyn Progress,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<CloneOutcome, CloneError> {
    let dir = opts
        .dest
        .to_str()
        .ok_or_else(|| CloneError::Library(Error::Message("non-UTF-8 destination path".into())))?;
    if opts.dest.is_dir()
        && opts
            .dest
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    {
        return Err(CloneError::DestNotEmpty(dir.to_owned()));
    }

    let repo = init_repository(
        &opts.dest,
        false,
        &opts.initial_branch,
        None,
        RefStorageFormat::default(),
    )
    .map_err(|e| {
        CloneError::Library(Error::Message(format!("could not initialize '{dir}': {e}")))
    })?;

    let origin_url = stored_clone_remote_url(&opts.url);
    set_local_config(
        &repo,
        &[
            (
                &format!("remote.{}.url", opts.remote_name),
                origin_url.clone(),
            ),
            (
                &format!("remote.{}.fetch", opts.remote_name),
                format!("+refs/heads/*:refs/remotes/{}/*", opts.remote_name),
            ),
        ],
    )?;

    let config = ConfigSet::load(&opts.environment, Some(&repo.git_dir), true)
        .map_err(CloneError::Library)?;
    let remote = Remote::from_config(&config, &opts.remote_name)?;
    let clone_message = format!("clone: from {}", opts.url);
    let identity =
        fetch_operation_identity(&opts.environment, &repo.git_dir).map_err(CloneError::Library)?;
    let clone_reflog = CloneReflog {
        identity: identity.clone(),
        message: clone_message.clone(),
    };
    let fetch_outcome = remote.fetch(
        &repo,
        FetchOptions {
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some(opts.remote_name.clone()),
            clone_reflog: Some(clone_reflog),
            ..Default::default()
        },
        progress,
        http_factory,
    )?;

    let default = fetch_outcome
        .default_branch
        .as_deref()
        .map(|d| d.strip_prefix("refs/heads/").unwrap_or(d).to_owned())
        .or_else(|| pick_default_branch(&repo, &opts.remote_name))
        .ok_or_else(|| {
            CloneError::Library(Error::Message(
                "the remote has no branches to check out".into(),
            ))
        })?;

    let tracking = format!("refs/remotes/{}/{default}", opts.remote_name);
    let oid = refs::resolve_ref(&repo.git_dir, &tracking).map_err(CloneError::Library)?;

    let branch_ref = format!("refs/heads/{default}");
    refs::write_ref(&repo.git_dir, &branch_ref, &oid).map_err(CloneError::Library)?;
    let zero = ObjectId::zero();
    refs::append_reflog(
        &repo.git_dir,
        &branch_ref,
        &zero,
        &oid,
        &identity,
        &clone_message,
        false,
    )
    .map_err(CloneError::Library)?;
    refs::write_symbolic_ref(&repo.git_dir, "HEAD", &branch_ref).map_err(CloneError::Library)?;
    refs::append_reflog(
        &repo.git_dir,
        "HEAD",
        &zero,
        &oid,
        &identity,
        &clone_message,
        false,
    )
    .map_err(CloneError::Library)?;
    set_local_config(
        &repo,
        &[
            (
                &format!("branch.{default}.remote"),
                opts.remote_name.clone(),
            ),
            (
                &format!("branch.{default}.merge"),
                format!("refs/heads/{default}"),
            ),
        ],
    )?;

    Ok(CloneOutcome {
        url: opts.url.clone(),
        path: opts.dest.clone(),
        branch: default,
        checkout_oid: oid,
        repo,
    })
}

fn stored_clone_remote_url(url: &str) -> String {
    if should_store_absolute_local_clone_url(url) {
        absolute_local_clone_source_url(Path::new(url.trim()))
    } else {
        url.to_owned()
    }
}

fn set_local_config(repo: &Repository, entries: &[(&str, String)]) -> Result<()> {
    let path = repo.git_dir.join("config");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    for (key, value) in entries {
        config.set(key, value)?;
    }
    config.write()?;
    Ok(())
}

fn pick_default_branch(repo: &Repository, remote_name: &str) -> Option<String> {
    for candidate in ["main", "master"] {
        if refs::resolve_ref(
            &repo.git_dir,
            &format!("refs/remotes/{remote_name}/{candidate}"),
        )
        .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    refs::list_refs(&repo.git_dir, &format!("refs/remotes/{remote_name}/"))
        .ok()?
        .into_iter()
        .find_map(|(name, _)| {
            name.strip_prefix(&format!("refs/remotes/{remote_name}/"))
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
}

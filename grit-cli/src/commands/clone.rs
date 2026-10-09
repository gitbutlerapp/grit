//! `grit clone` — copy a remote repository into a new directory.
//!
//! Composed from the pieces `grit` already has: initialize a repo, point `origin`
//! at the source, fetch, then check out the remote's default branch.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::refs;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::transfer::{CloneReflog, FetchOptions, TagMode};
use grit_lib::transport_path::{
    absolute_local_clone_source_url, should_store_absolute_local_clone_url,
};
use serde::Serialize;

use crate::context;
use crate::net;
use crate::output::{progress, HumanRender, OutputMode};

/// Result of `grit clone`.
#[derive(Serialize)]
pub struct CloneOutcome {
    pub url: String,
    /// Destination directory.
    pub path: String,
    /// Default branch checked out.
    pub branch: String,
}

impl HumanRender for CloneOutcome {
    fn render_human(&self) {
        println!("Cloned into '{}' on branch {}.", self.path, self.branch);
    }
}

pub fn run(url: &str, dir: Option<String>, mode: OutputMode) -> Result<CloneOutcome> {
    let dir = dir.unwrap_or_else(|| derive_dir(url));
    let path = PathBuf::from(&dir);
    let dest_preexisted_empty = path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
    if path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    {
        bail!("destination '{dir}' already exists and is not empty");
    }

    progress(mode, &format!("Cloning into '{dir}' ..."));
    match clone_into(url, &path, &dir) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            cleanup_failed_clone(&path, dest_preexisted_empty);
            Err(err)
        }
    }
}

fn clone_into(url: &str, path: &Path, dir: &str) -> Result<CloneOutcome> {
    let repo = init_repository(path, false, "main", None, "files")
        .with_context(|| format!("could not initialize '{dir}'"))?;

    let origin_url = stored_clone_remote_url(url);
    set_config(
        &repo,
        &[
            ("remote.origin.url", origin_url),
            (
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*".to_owned(),
            ),
        ],
    )?;

    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    let refspecs = net::fetch_refspecs(&config, net::DEFAULT_REMOTE);
    let clone_message = format!("clone: from {url}");
    let identity =
        grit_lib::fetch::fetch_operation_identity(&crate::context::environment(), &repo.git_dir)
            .context("could not build clone reflog identity")?;
    let clone_reflog = CloneReflog {
        identity: identity.clone(),
        message: clone_message.clone(),
    };
    let outcome = net::fetch_with_options(
        &repo,
        &config,
        net::DEFAULT_REMOTE,
        refspecs,
        FetchOptions {
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some(net::DEFAULT_REMOTE.to_owned()),
            clone_reflog: Some(clone_reflog),
            ..Default::default()
        },
    )
    .context("could not fetch from the remote")?;

    let default = outcome
        .default_branch
        .as_deref()
        .map(|d| d.strip_prefix("refs/heads/").unwrap_or(d).to_owned())
        .or_else(|| pick_default_branch(&repo))
        .context("the remote has no branches to check out")?;

    let tracking = format!("refs/remotes/origin/{default}");
    let oid = refs::resolve_ref(&repo.git_dir, &tracking)
        .with_context(|| format!("remote default branch '{default}' not found after fetch"))?;

    let branch_ref = format!("refs/heads/{default}");
    refs::write_ref(&repo.git_dir, &branch_ref, &oid).context("could not create local branch")?;
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
    .context("could not write branch reflog")?;
    refs::write_symbolic_ref(&repo.git_dir, "HEAD", &branch_ref).context("could not set HEAD")?;
    refs::append_reflog(
        &repo.git_dir,
        "HEAD",
        &zero,
        &oid,
        &identity,
        &clone_message,
        false,
    )
    .context("could not write HEAD reflog")?;
    set_config(
        &repo,
        &[
            (&format!("branch.{default}.remote"), "origin".to_owned()),
            (
                &format!("branch.{default}.merge"),
                format!("refs/heads/{default}"),
            ),
        ],
    )?;

    let tree = context::commit_tree(&repo, &oid)?;
    checkout_between_trees(&repo, None, &tree).context("could not check out files")?;

    Ok(CloneOutcome {
        url: url.to_owned(),
        path: dir.to_owned(),
        branch: default,
    })
}

/// Remove a partially created clone destination (mirrors grit-git `remove_junk_path`).
fn cleanup_failed_clone(path: &Path, keep_toplevel: bool) {
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

/// URL stored in `remote.origin.url` after clone — absolute for local paths so later
/// fetch/push resolve against the repository, not the process cwd.
fn stored_clone_remote_url(url: &str) -> String {
    if should_store_absolute_local_clone_url(url) {
        absolute_local_clone_source_url(Path::new(url.trim()))
    } else {
        url.to_owned()
    }
}

/// Derive a destination directory from a clone URL (the last path component,
/// minus a trailing `.git`). Handles `https://`, `scp`-style, and local paths.
fn derive_dir(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

/// Apply a set of key/value pairs to the repository's local config file.
fn set_config(repo: &Repository, entries: &[(&str, String)]) -> Result<()> {
    let path = repo.git_dir.join("config");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)
        .context("could not parse repository config")?;
    for (key, value) in entries {
        config.set(key, value)?;
    }
    config
        .write()
        .context("could not write repository config")?;
    Ok(())
}

/// Fall back to a sensible default branch when the remote didn't advertise one.
fn pick_default_branch(repo: &Repository) -> Option<String> {
    for candidate in ["main", "master"] {
        if refs::resolve_ref(&repo.git_dir, &format!("refs/remotes/origin/{candidate}")).is_ok() {
            return Some(candidate.to_owned());
        }
    }
    refs::list_refs(&repo.git_dir, "refs/remotes/origin/")
        .ok()?
        .into_iter()
        .find_map(|(name, _)| {
            name.strip_prefix("refs/remotes/origin/")
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
}

//! `grit clone` — copy a remote repository into a new directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use grit_lib::clone::{
    cleanup_failed_clone, clone as clone_repo, derive_clone_dir, dest_preexisted_empty,
    CloneOptions,
};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::remote::DefaultHttpClientFactory;
use serde::Serialize;

use crate::context;
use crate::output::{progress, HumanRender, MarkdownRender, OutputMode};

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

impl MarkdownRender for CloneOutcome {}

pub fn run(url: &str, dir: Option<String>, mode: OutputMode) -> Result<CloneOutcome> {
    let dir = dir.unwrap_or_else(|| derive_clone_dir(url));
    let path = PathBuf::from(&dir);
    let dest_preexisted_empty = dest_preexisted_empty(&path);

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
    let factory = DefaultHttpClientFactory;
    let outcome = clone_repo(
        &CloneOptions {
            url: url.to_owned(),
            dest: path.to_path_buf(),
            environment: crate::context::environment(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
            initial_branch: "main".to_owned(),
        },
        &mut grit_lib::fetch::NoProgress,
        Some(&factory),
    )
    .map_err(|e| anyhow::Error::msg(e.to_string()))?;

    let tree = context::commit_tree(&outcome.repo, &outcome.checkout_oid)?;
    checkout_between_trees(&outcome.repo, None, &tree).context("could not check out files")?;

    Ok(CloneOutcome {
        url: url.to_owned(),
        path: dir.to_owned(),
        branch: outcome.branch,
    })
}

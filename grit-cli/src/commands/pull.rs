//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::porcelain::worktree_guard::prepare_tree_checkout;
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::state::{resolve_head, HeadState};

use crate::commands::merge::{self, MergeOutcome};
use crate::context;
use grit_lib::fetch::NoProgress;
use grit_lib::remote::{DefaultHttpClientFactory, Remote, DEFAULT_REMOTE};
use grit_lib::transfer::{FetchOptions, TagMode};

pub fn run() -> Result<MergeOutcome> {
    let repo = context::discover()?;

    let model = status(&repo, &StatusOptions::default(), &mut NullProgress)
        .context("could not compute status")?;
    if !model.staged.is_empty() || !model.unstaged.is_empty() {
        bail!("you have uncommitted changes — commit them before pulling");
    }

    let (refname, short_name, head_oid) = match resolve_head(&repo.git_dir)? {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        HeadState::Detached { .. } => bail!("HEAD is detached; grit pull needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    let remote = config
        .get(&format!("branch.{short_name}.remote"))
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());

    let factory = DefaultHttpClientFactory;
    Remote::from_config(&config, &remote)
        .map_err(|e| anyhow::Error::msg(e.to_string()))?
        .fetch(
            &repo,
            FetchOptions {
                tags: TagMode::Following,
                ..Default::default()
            },
            &mut NoProgress,
            Some(&factory),
        )
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;

    // Which upstream tracking ref to integrate.
    let upstream_branch = config
        .get(&format!("branch.{short_name}.merge"))
        .and_then(|m| {
            m.strip_prefix("refs/heads/")
                .map(str::to_owned)
                .or(Some(m.clone()))
        })
        .unwrap_or_else(|| short_name.clone());
    let upstream_ref = format!("refs/remotes/{remote}/{upstream_branch}");
    let upstream_oid = refs::resolve_ref(&repo.git_dir, &upstream_ref)
        .with_context(|| format!("no upstream tracking ref {upstream_ref} after fetch"))?;

    match head_oid {
        Some(head_oid) => merge::integrate(
            &repo,
            &refname,
            head_oid,
            upstream_oid,
            &format!("{remote}/{upstream_branch}"),
        ),
        None => {
            // Unborn branch: adopt the upstream as the first commit.
            let upstream_tree = context::commit_tree(&repo, &upstream_oid)?;
            prepare_tree_checkout(&repo, None, &upstream_tree)
                .map_err(anyhow::Error::new)
                .context("could not verify working tree")?;
            checkout_between_trees(&repo, None, &upstream_tree)
                .context("could not populate the working tree")?;
            refs::write_ref(&repo.git_dir, &refname, &upstream_oid)
                .context("could not set branch")?;
            Ok(MergeOutcome::set_upstream(
                &short_name,
                &format!("{remote}/{upstream_branch}"),
                upstream_oid,
            ))
        }
    }
}

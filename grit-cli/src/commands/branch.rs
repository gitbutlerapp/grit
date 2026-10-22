//! `grit branch` — list branches, or create / delete / rename / configure one.

use anyhow::{bail, Context, Result};
use grit_lib::error::Error;
use grit_lib::porcelain::branch::{
    create_branch, delete_branch, list_branches, rename_branch, set_upstream, unset_upstream,
    upstream_tracking_display, CreateBranchOptions, DeleteMode, RenameBranchOptions, StartPoint,
    TrackMode,
};
use grit_lib::refs::branch_tracking::upstream_branch;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, resolve_head_for_repository, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::json_error::{self, StructuredCliError, StructuredJsonError};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Parsed flags for a single `grit branch` invocation.
pub struct BranchRunOptions {
    /// Branch name for create, delete, or upstream (omit to list).
    pub name: Option<String>,
    /// Start commit for create (`grit branch <name> <start>`).
    pub start: Option<String>,
    /// Overwrite an existing branch on create or rename.
    pub force: bool,
    /// Always configure upstream from the start point when creating.
    pub track: bool,
    /// Never configure upstream when creating.
    pub no_track: bool,
    /// Set `branch.<name>.remote` / `.merge` from this upstream ref.
    pub set_upstream_to: Option<String>,
    /// Clear upstream configuration for a branch.
    pub unset_upstream: bool,
    /// `-m` / `--move` arguments (`[old] new`).
    pub rename: Option<Vec<String>>,
    /// `-M`: move with `--force` (or `grit branch -M <new>` on the current branch).
    pub force_move: bool,
    pub delete: bool,
    pub force_delete: bool,
    pub dry_run: bool,
}

/// Result of `grit branch`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BranchOutcome {
    List {
        /// The current branch, or `null` when detached / unborn.
        current: Option<String>,
        branches: Vec<BranchEntry>,
    },
    Create {
        name: String,
        oid: String,
        short_oid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    Delete {
        name: String,
        oid: String,
        short_oid: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    Rename {
        old: String,
        new: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        oid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        short_oid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        replaced: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    SetUpstream {
        branch: String,
        upstream: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
    UnsetUpstream {
        branch: String,
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        dry_run: bool,
    },
}

/// One branch in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct BranchEntry {
    pub name: String,
    pub current: bool,
}

impl HumanRender for BranchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List { branches, .. } => {
                if branches.is_empty() {
                    outln!(w, "No branches yet.");
                    return Ok(());
                }
                for branch in branches {
                    if branch.current {
                        outln!(w, "* {}", branch.name);
                    } else {
                        outln!(w, "  {}", branch.name);
                    }
                }
            }
            BranchOutcome::Create {
                name,
                short_oid,
                upstream,
                dry_run,
                ..
            } => {
                if *dry_run {
                    outln!(w, "Would create branch {name} at {short_oid}");
                } else {
                    outln!(w, "Created branch {name} at {short_oid}");
                }
                if let Some(up) = upstream {
                    outln!(w, "  tracking {up}");
                }
            }
            BranchOutcome::Delete {
                name,
                short_oid,
                dry_run,
                ..
            } => {
                if *dry_run {
                    outln!(w, "Would delete branch {name} (was {short_oid}).");
                } else {
                    outln!(w, "Deleted branch {name} (was {short_oid}).");
                }
            }
            BranchOutcome::Rename {
                old, new, dry_run, ..
            } => {
                if *dry_run {
                    outln!(w, "Would rename branch {old} to {new}");
                } else {
                    outln!(w, "Renamed branch {old} to {new}");
                }
            }
            BranchOutcome::SetUpstream {
                branch,
                upstream,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would set upstream of {branch} to {upstream}");
                } else {
                    outln!(w, "Set upstream of {branch} to {upstream}");
                }
            }
            BranchOutcome::UnsetUpstream { branch, dry_run } => {
                if *dry_run {
                    outln!(w, "Would unset upstream of {branch}");
                } else {
                    outln!(w, "Unset upstream of {branch}");
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for BranchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List { branches, .. } => markdown::print_branch_list(w, branches)?,
            BranchOutcome::Create {
                name,
                short_oid,
                upstream,
                dry_run,
                ..
            } => {
                let verb = if *dry_run { "Would create" } else { "Created" };
                if let Some(up) = upstream {
                    outln!(
                        w,
                        "{verb} branch `{name}` at `{short_oid}` (tracking `{up}`)."
                    );
                } else {
                    outln!(w, "{verb} branch `{name}` at `{short_oid}`.");
                }
            }
            BranchOutcome::Delete {
                name,
                short_oid,
                dry_run,
                ..
            } => {
                let verb = if *dry_run { "Would delete" } else { "Deleted" };
                outln!(w, "{verb} branch `{name}` (was `{short_oid}`).");
            }
            BranchOutcome::Rename {
                old, new, dry_run, ..
            } => {
                let verb = if *dry_run { "Would rename" } else { "Renamed" };
                outln!(w, "{verb} branch `{old}` to `{new}`.");
            }
            BranchOutcome::SetUpstream {
                branch,
                upstream,
                dry_run,
            } => {
                let verb = if *dry_run { "Would set" } else { "Set" };
                outln!(w, "{verb} upstream of `{branch}` to `{upstream}`.");
            }
            BranchOutcome::UnsetUpstream { branch, dry_run } => {
                let verb = if *dry_run { "Would unset" } else { "Unset" };
                outln!(w, "{verb} upstream of `{branch}`.");
            }
        }
        Ok(())
    }
}

/// List, create, delete, rename, or configure a branch depending on `opts`.
pub fn run(opts: BranchRunOptions) -> Result<BranchOutcome> {
    if opts.delete && opts.force {
        bail!(
            "`--force` does not delete branches; use `--force-delete` (or `-D`) when the branch is not fully merged"
        );
    }
    if opts.delete && (opts.force_move || opts.rename.is_some()) {
        bail!("cannot combine `--delete` with `--move`");
    }
    if opts.unset_upstream && opts.set_upstream_to.is_some() {
        bail!("cannot combine `--unset-upstream` with `--set-upstream-to`");
    }
    if opts.start.is_some()
        && (opts.delete
            || opts.unset_upstream
            || opts.set_upstream_to.is_some()
            || opts.rename.is_some()
            || opts.force_move)
    {
        bail!("unexpected start commit with this operation");
    }

    let repo = context::discover()?;

    if opts.unset_upstream {
        return unset(&repo, &branch_for_upstream(&repo, &opts)?, opts.dry_run);
    }
    if let Some(upstream) = &opts.set_upstream_to {
        return set(
            &repo,
            &branch_for_upstream(&repo, &opts)?,
            upstream,
            opts.dry_run,
        );
    }
    if let Some((old, new, force)) = resolve_rename(&repo, &opts)? {
        return rename(&repo, &old, &new, force, opts.dry_run);
    }
    if opts.delete || opts.force_delete {
        let name = opts
            .name
            .ok_or_else(|| anyhow::anyhow!("branch name required with `--delete`"))?;
        return remove(&repo, &name, opts.force_delete, opts.dry_run);
    }
    match (&opts.name, &opts.start) {
        (None, None) => list(&repo),
        (Some(name), start) => create(&repo, name, start.as_deref(), &opts),
        (None, Some(_)) => bail!("branch name required before the start commit"),
    }
}

fn branch_for_upstream(repo: &Repository, opts: &BranchRunOptions) -> Result<String> {
    if let Some(name) = &opts.name {
        return Ok(name.clone());
    }
    current_branch_short(repo)
}

fn current_branch_short(repo: &Repository) -> Result<String> {
    match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } => Ok(short_name),
        HeadState::Detached { .. } => bail!("HEAD is detached; pass the branch name"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }
}

fn resolve_rename(
    repo: &Repository,
    opts: &BranchRunOptions,
) -> Result<Option<(String, String, bool)>> {
    let force = opts.force || opts.force_move;
    if let Some(args) = &opts.rename {
        let current = current_branch_short(repo).ok();
        return Ok(Some(match args.len() {
            1 => {
                let old = if let Some(name) = &opts.name {
                    name.clone()
                } else {
                    current.ok_or_else(|| {
                        anyhow::anyhow!("branch name required when HEAD is not on a branch")
                    })?
                };
                (old, args[0].clone(), force)
            }
            2 => (args[0].clone(), args[1].clone(), force),
            _ => bail!("`--move` expects one or two branch names"),
        }));
    }
    if opts.force_move {
        let new = opts
            .name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("new branch name required with `-M`"))?;
        let old = current_branch_short(repo)?;
        return Ok(Some((old, new, true)));
    }
    Ok(None)
}

fn list(repo: &Repository) -> Result<BranchOutcome> {
    let listing = list_branches(repo).context("could not list branches")?;
    Ok(BranchOutcome::List {
        current: listing.current,
        branches: listing
            .branches
            .into_iter()
            .map(|branch| BranchEntry {
                name: branch.name,
                current: branch.current,
            })
            .collect(),
    })
}

fn track_mode(opts: &BranchRunOptions) -> TrackMode {
    if opts.no_track {
        TrackMode::Never
    } else if opts.track {
        TrackMode::Always
    } else {
        TrackMode::Auto
    }
}

fn create(
    repo: &Repository,
    name: &str,
    start: Option<&str>,
    opts: &BranchRunOptions,
) -> Result<BranchOutcome> {
    let start_point = if let Some(rev) = start {
        let oid = resolve_revision(repo, rev).map_err(map_branch_error)?;
        Some(StartPoint {
            oid,
            label: rev.to_owned(),
        })
    } else {
        None
    };

    let create_opts = CreateBranchOptions {
        name: name.to_owned(),
        start: start_point,
        force: opts.force,
        track: track_mode(opts),
        dry_run: opts.dry_run,
    };
    let created = create_branch(repo, &create_opts).map_err(map_branch_error)?;
    let oid = created.oid.to_hex();
    let short_oid = short_hex(&oid);
    let upstream = if opts.dry_run {
        predicted_upstream(repo, name, &create_opts)?
    } else {
        upstream_branch(repo, name).map(|base| base.name)
    };
    Ok(BranchOutcome::Create {
        name: created.name,
        oid,
        short_oid,
        upstream,
        dry_run: opts.dry_run,
    })
}

fn predicted_upstream(
    repo: &Repository,
    name: &str,
    opts: &CreateBranchOptions,
) -> Result<Option<String>> {
    use grit_lib::porcelain::branch::resolve_create_tracking;
    let (start_oid, label) = match &opts.start {
        Some(point) => (point.oid, point.label.clone()),
        None => {
            let head = resolve_head(repo.git_dir())?;
            let oid = head.oid().copied().ok_or_else(|| {
                map_branch_error(Error::Branch(
                    grit_lib::porcelain::branch::BranchError::NoCommits,
                ))
            })?;
            let label =
                grit_lib::porcelain::checkout_reflog::branch_create_reflog_source_label(&head);
            (oid, label)
        }
    };
    let tracking = resolve_create_tracking(repo, name, &label, start_oid, opts.track)
        .map_err(map_branch_error)?;
    Ok(tracking.map(|(remote, merge)| upstream_tracking_display(&remote, &merge)))
}

fn remove(
    repo: &Repository,
    name: &str,
    force_delete: bool,
    dry_run: bool,
) -> Result<BranchOutcome> {
    let mode = if force_delete {
        DeleteMode::Force
    } else {
        DeleteMode::Safe
    };
    let deleted = delete_branch(repo, name, mode, dry_run).map_err(map_branch_error)?;
    let oid = deleted.oid.to_hex();
    let short_oid = short_hex(&oid);
    Ok(BranchOutcome::Delete {
        name: deleted.name,
        oid,
        short_oid,
        dry_run,
    })
}

fn rename(
    repo: &Repository,
    old: &str,
    new: &str,
    force: bool,
    dry_run: bool,
) -> Result<BranchOutcome> {
    let outcome = rename_branch(
        repo,
        &RenameBranchOptions {
            old: old.to_owned(),
            new: new.to_owned(),
            force,
            dry_run,
        },
    )
    .map_err(map_branch_error)?;
    Ok(BranchOutcome::Rename {
        old: outcome.old,
        new: outcome.new,
        oid: outcome.oid.map(|o| o.to_hex()),
        short_oid: outcome.oid.map(|o| short_hex(&o.to_hex())),
        replaced: outcome.replaced.map(|o| o.to_hex()),
        dry_run: outcome.dry_run,
    })
}

fn set(repo: &Repository, branch: &str, upstream: &str, dry_run: bool) -> Result<BranchOutcome> {
    if dry_run {
        return Ok(BranchOutcome::SetUpstream {
            branch: branch.to_owned(),
            upstream: upstream.to_owned(),
            dry_run: true,
        });
    }
    set_upstream(repo, branch, upstream).map_err(map_branch_error)?;
    let display = upstream_branch(repo, branch)
        .map(|base| base.name)
        .unwrap_or_else(|| upstream.to_owned());
    Ok(BranchOutcome::SetUpstream {
        branch: branch.to_owned(),
        upstream: display,
        dry_run: false,
    })
}

fn unset(repo: &Repository, branch: &str, dry_run: bool) -> Result<BranchOutcome> {
    if !dry_run {
        unset_upstream(repo, branch).map_err(map_branch_error)?;
    }
    Ok(BranchOutcome::UnsetUpstream {
        branch: branch.to_owned(),
        dry_run,
    })
}

fn short_hex(full: &str) -> String {
    full[..7.min(full.len())].to_owned()
}

fn branch_error_kind(err: &grit_lib::porcelain::branch::BranchError) -> &'static str {
    use grit_lib::porcelain::branch::BranchError;
    match err {
        BranchError::AlreadyExists { .. } => "branch_already_exists",
        BranchError::NotFound { .. } => "branch_not_found",
        BranchError::InvalidName { .. } => "branch_invalid_name",
        BranchError::NotFullyMerged { .. } => "branch_not_fully_merged",
        BranchError::CheckedOut { .. } => "branch_checked_out",
        BranchError::NoCommits => "branch_no_commits",
        BranchError::InvalidStartPoint { .. } => "branch_invalid_start_point",
        BranchError::UpstreamNotTracking { .. } => "branch_upstream_not_tracking",
        BranchError::NoUpstream { .. } => "branch_no_upstream",
        _ => "branch_error",
    }
}

fn branch_error_message(err: &grit_lib::porcelain::branch::BranchError) -> String {
    use grit_lib::porcelain::branch::BranchError;
    match err {
        BranchError::NotFullyMerged { name, .. } => format!(
            "{err}\nIf you are sure you want to delete it, run 'grit branch --force-delete {name}'"
        ),
        BranchError::AlreadyExists { name } => {
            format!("branch '{name}' already exists (use --force to replace its tip)")
        }
        other => other.to_string(),
    }
}

pub fn map_branch_error(err: Error) -> anyhow::Error {
    match err {
        Error::Branch(branch_err) => {
            let human = branch_error_message(&branch_err);
            anyhow::Error::new(StructuredCliError {
                payload: StructuredJsonError {
                    error: human.clone(),
                    kind: branch_error_kind(&branch_err),
                    conflicts: None,
                    would_overwrite: None,
                    oid: None,
                },
                human,
            })
        }
        Error::BranchCheckedOutInWorktree { worktree_path, .. } => {
            let human = format!("branch is checked out in '{worktree_path}'");
            anyhow::Error::new(StructuredCliError {
                payload: StructuredJsonError {
                    error: human.clone(),
                    kind: "branch_checked_out",
                    conflicts: None,
                    would_overwrite: None,
                    oid: None,
                },
                human,
            })
        }
        other => json_error::map_lib_error(other),
    }
}

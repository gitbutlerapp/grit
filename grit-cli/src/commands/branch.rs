//! `grit branch` — list branches, or create / delete / rename / configure one.

use anyhow::{bail, Context, Error as AnyhowError, Result};
use grit_lib::error::Error;
use grit_lib::error::ObjectError;
use grit_lib::error::RevisionError;
use grit_lib::porcelain::branch::{
    BranchCreateOptions, BranchDeleteMode, BranchDeleteOptions, BranchRenameOptions, StartPoint,
    TrackMode,
};
use grit_lib::refs::branch_tracking::{BranchDetail, BranchListQuery, BranchScope, BranchSummary};
use grit_lib::refs::listing::{RefQuery, RefSort, SortKey};
use grit_lib::repo::Repository;
use grit_lib::revwalk::ResolveOptions;

use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_lib::state::{HeadState, InProgressOperation};
use std::error::Error as StdError;
use std::fmt;
use time::format_description::well_known::Rfc3339;

use crate::branch_list_args::BranchListArgs;
use crate::context;
use crate::error_kind::ErrorKind;
use crate::json_error::{self, StructuredCliError, StructuredJsonError};
pub use crate::render::branch::{BranchEntry, BranchListScope, BranchOutcome, BranchUpstreamEntry};

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
    /// `-M`: force rename (`-M <new>` or `-M <old> <new>`).
    pub force_rename: Option<Vec<String>>,
    pub delete: bool,
    pub force_delete: bool,
    pub list: BranchListArgs,
    pub list_full_detail: bool,
    pub dry_run: bool,
}

/// Usage mistakes specific to `grit branch` (exit code 2).
#[derive(Debug)]
pub struct BranchUsageError;

impl fmt::Display for BranchUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid branch arguments")
    }
}

impl StdError for BranchUsageError {}

fn branch_usage(message: impl Into<String>) -> AnyhowError {
    let human = message.into();
    AnyhowError::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::Usage),
        human,
    })
    .context(BranchUsageError)
}

/// List, create, delete, rename, or configure a branch depending on `opts`.
pub fn run(opts: BranchRunOptions) -> Result<BranchOutcome> {
    if (opts.delete || opts.force_delete) && opts.force {
        return Err(branch_usage(
            "`--force` does not delete branches; use `--force-delete` (or `-D`) when the branch is not fully merged",
        ));
    }
    let deleting = opts.delete || opts.force_delete;
    if (opts.delete || opts.force_delete) && (opts.rename.is_some() || opts.force_rename.is_some())
    {
        return Err(branch_usage("cannot combine `--delete` with `--move`"));
    }
    if deleting && opts.unset_upstream {
        return Err(branch_usage(
            "cannot combine `--delete` with `--unset-upstream`",
        ));
    }
    if deleting && opts.set_upstream_to.is_some() {
        return Err(branch_usage(
            "cannot combine `--delete` with `--set-upstream-to`",
        ));
    }
    if opts.unset_upstream && opts.set_upstream_to.is_some() {
        bail!("cannot combine `--unset-upstream` with `--set-upstream-to`");
    }
    if opts.start.is_some()
        && (deleting
            || opts.unset_upstream
            || opts.set_upstream_to.is_some()
            || opts.rename.is_some()
            || opts.force_rename.is_some())
    {
        return Err(branch_usage("unexpected start commit with this operation"));
    }
    if branch_mutation_count(&opts) > 1 {
        return Err(branch_usage(
            "cannot combine branch delete, rename, upstream, and create in one invocation",
        ));
    }
    if opts.list.remotes && opts.list.all {
        return Err(branch_usage("cannot combine `--remotes` with `--all`"));
    }
    if list_args_active(&opts.list)
        && (opts.name.is_some() || opts.start.is_some() || branch_mutation_count(&opts) > 0)
    {
        return Err(branch_usage(
            "list filters apply only when listing branches (omit the branch name)",
        ));
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
            .ok_or_else(|| branch_usage("branch name required with `--delete`"))?;
        return remove(&repo, &name, opts.force_delete, opts.dry_run);
    }
    match (&opts.name, &opts.start) {
        (None, None) => list(&repo, &opts),
        (Some(name), start) => create(&repo, name, start.as_deref(), &opts),
        (None, Some(_)) => Err(branch_usage("branch name required before the start commit")),
    }
}

fn list_args_active(args: &BranchListArgs) -> bool {
    args.remotes
        || args.all
        || args.limit.get().is_some()
        || !args.patterns.is_empty()
        || args.merged.is_some()
        || args.no_merged.is_some()
        || args.contains.is_some()
        || args.sort.is_some()
        || args.reverse
        || args.show_current
}

/// Count mutually exclusive branch mutations requested in one invocation.
fn branch_mutation_count(opts: &BranchRunOptions) -> u32 {
    let mut n = 0u32;
    if opts.delete || opts.force_delete {
        n += 1;
    }
    if opts.rename.is_some() || opts.force_rename.is_some() {
        n += 1;
    }
    if opts.unset_upstream || opts.set_upstream_to.is_some() {
        n += 1;
    }
    let creates = opts.name.is_some()
        && !opts.delete
        && !opts.force_delete
        && opts.rename.is_none()
        && opts.force_rename.is_none()
        && !opts.unset_upstream
        && opts.set_upstream_to.is_none();
    if creates {
        n += 1;
    }
    n
}

fn branch_for_upstream(repo: &Repository, opts: &BranchRunOptions) -> Result<String> {
    if let Some(name) = &opts.name {
        return Ok(name.clone());
    }
    current_branch_short(repo)
}

fn current_branch_short(repo: &Repository) -> Result<String> {
    match repo.references().head()? {
        HeadState::Branch { short_name, .. } => Ok(short_name),
        HeadState::Detached { .. } => bail!("HEAD is detached; pass the branch name"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }
}

fn resolve_rename(
    repo: &Repository,
    opts: &BranchRunOptions,
) -> Result<Option<(String, String, bool)>> {
    let (args, force) = if let Some(args) = &opts.force_rename {
        (args.as_slice(), true)
    } else if let Some(args) = &opts.rename {
        (args.as_slice(), opts.force)
    } else {
        return Ok(None);
    };
    let current = current_branch_short(repo).ok();
    let parsed = match args.len() {
        1 => {
            let old = if let Some(name) = &opts.name {
                name.clone()
            } else if let Some(name) = current {
                name
            } else if let Some(op) = repo.operation().blocking_switch()? {
                return Err(json_error::operation_in_progress_for_repo(
                    repo,
                    "rename the branch",
                    op,
                ));
            } else {
                bail!("branch name required when HEAD is not on a branch");
            };
            (old, args[0].clone(), force)
        }
        2 => (args[0].clone(), args[1].clone(), force),
        _ => bail!("`--move` expects one or two branch names"),
    };
    if let Some(op) = repo.operation().blocking_switch()? {
        if matches!(
            op,
            InProgressOperation::Rebase | InProgressOperation::RebaseInteractive
        ) && rebase_branch_short_name(repo)? == Some(parsed.0.clone())
        {
            return Err(json_error::operation_in_progress_for_repo(
                repo,
                "rename the branch",
                op,
            ));
        }
    }
    Ok(Some(parsed))
}

fn rebase_branch_short_name(repo: &Repository) -> Result<Option<String>> {
    let state = repo.operation().status_state()?;
    Ok(state.rebase_branch)
}

fn list(repo: &Repository, opts: &BranchRunOptions) -> Result<BranchOutcome> {
    let list = &opts.list;
    let scope = if list.all {
        BranchScope::All
    } else if list.remotes {
        BranchScope::Remote
    } else {
        BranchScope::Local
    };
    let query = BranchListQuery {
        scope,
        refs: build_ref_query(repo, list)?,
    };
    let detail = if opts.list_full_detail {
        BranchDetail::Full
    } else {
        BranchDetail::Names
    };
    let limit = list.limit.get();
    let rows = repo
        .branches()
        .list(&query, detail)
        .context("could not list branches")?;
    let limited = grit_lib::list_limit::Limited::from_vec(rows, limit);
    let rows = limited.items;
    let truncated = limited.truncated;
    let current = match repo.references().head()? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        HeadState::Detached { .. } | HeadState::Invalid => None,
    };
    let list_scope = match scope {
        BranchScope::Local => BranchListScope::Local,
        BranchScope::Remote => BranchListScope::Remote,
        // `All` and scopes this CLI does not know list everything.
        _ => BranchListScope::All,
    };
    Ok(BranchOutcome::List {
        current,
        show_current: list.show_current,
        branches: rows
            .into_iter()
            .map(|row| summary_to_entry(row, opts.list_full_detail))
            .collect(),
        truncated,
        scope: list_scope,
    })
}

fn build_ref_query(repo: &Repository, args: &BranchListArgs) -> Result<RefQuery> {
    let sort_key = match args.sort.as_deref() {
        None | Some("name") | Some("refname") => SortKey::Name,
        Some("version") | Some("versionrefname") => SortKey::Version,
        Some("date") | Some("committerdate") => SortKey::Date,
        Some(other) => {
            return Err(branch_usage(format!(
                "unknown sort key '{other}' (expected name, version, or date)"
            )));
        }
    };
    Ok(RefQuery {
        patterns: args.patterns.clone(),
        sort: RefSort {
            key: sort_key,
            reverse: args.reverse,
        },
        limit: grit_lib::list_limit::fetch_plus_one(args.limit.get()),
        contains: resolve_filter_revs(repo, args.contains.as_deref())?,
        merged: resolve_filter_revs(repo, args.merged.as_deref())?,
        no_merged: resolve_filter_revs(repo, args.no_merged.as_deref())?,
        ..RefQuery::default()
    })
}

fn resolve_filter_revs(
    repo: &Repository,
    rev: Option<&str>,
) -> Result<Vec<grit_lib::objects::ObjectId>> {
    let Some(rev) = rev else {
        return Ok(Vec::new());
    };
    let oid = repo
        .revwalk()
        .resolve(rev, &ResolveOptions::default())
        .map_err(map_branch_error)?;
    Ok(vec![oid])
}

fn summary_to_entry(row: BranchSummary, full_detail: bool) -> BranchEntry {
    let oid_hex = row.oid.to_hex();
    let remote = remote_name_from_full_ref(&row.full_ref, row.is_remote);
    let mut entry = BranchEntry {
        name: row.name,
        current: row.current,
        is_remote: row.is_remote,
        full_ref: None,
        remote: None,
        oid: Some(oid_hex.clone()),
        short_oid: Some(row.oid.to_short_hex()),
        subject: None,
        date: None,
        upstream: row.upstream.as_ref().map(upstream_to_entry),
        worktree: None,
    };
    if full_detail {
        entry.full_ref = Some(row.full_ref);
        entry.remote = remote;
        entry.subject = row.subject;
        entry.date = row.committer_time.and_then(|t| t.format(&Rfc3339).ok());
        entry.worktree = row.worktree.map(|p| p.display().to_string());
    }
    entry
}

fn upstream_to_entry(up: &grit_lib::refs::branch_tracking::UpstreamStatus) -> BranchUpstreamEntry {
    BranchUpstreamEntry {
        ref_name: up.short.clone(),
        remote: Some(up.remote.clone()),
        gone: up.gone,
        ahead: up.ahead,
        behind: up.behind,
    }
}

fn remote_name_from_full_ref(full_ref: &str, is_remote: bool) -> Option<String> {
    if !is_remote {
        return None;
    }
    full_ref
        .strip_prefix("refs/remotes/")
        .and_then(|rest| rest.split('/').next())
        .map(str::to_owned)
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

fn map_create_start_revision_error(rev: &str, err: Error) -> AnyhowError {
    use grit_lib::porcelain::branch::BranchError;
    match err {
        Error::Revision(RevisionError::Parse(
            RevParseError::InvalidObjectName { .. } | RevParseError::AmbiguousArgument { .. },
        ))
        | Error::Object(ObjectError::NotFound { .. }) => {
            map_branch_error(Error::Branch(BranchError::InvalidStartPoint {
                label: rev.to_owned(),
                reason: "revision not found".into(),
            }))
        }
        other => map_branch_error(other),
    }
}

fn create(
    repo: &Repository,
    name: &str,
    start: Option<&str>,
    opts: &BranchRunOptions,
) -> Result<BranchOutcome> {
    let start_point = if let Some(rev) = start {
        let oid = repo
            .revwalk()
            .resolve(rev, &ResolveOptions::default())
            .map_err(|err| map_create_start_revision_error(rev, err))?;
        Some(StartPoint {
            oid,
            label: rev.to_owned(),
        })
    } else {
        None
    };

    let create_opts = BranchCreateOptions {
        name: name.to_owned(),
        start: start_point,
        force: opts.force,
        track: track_mode(opts),
        dry_run: opts.dry_run,
    };
    let created = repo
        .branches()
        .create(&create_opts)
        .map_err(map_branch_error)?;
    let oid = created.oid.to_hex();
    let short_oid = created.oid.to_short_hex();
    Ok(BranchOutcome::Create {
        name: created.name,
        oid,
        short_oid,
        upstream: created.upstream,
        dry_run: opts.dry_run,
    })
}

fn remove(
    repo: &Repository,
    name: &str,
    force_delete: bool,
    dry_run: bool,
) -> Result<BranchOutcome> {
    let mode = if force_delete {
        BranchDeleteMode::Force
    } else {
        BranchDeleteMode::Safe
    };
    let deleted = repo
        .branches()
        .delete(name, &BranchDeleteOptions { mode, dry_run })
        .map_err(map_branch_error)?;
    let oid = deleted.oid.to_hex();
    let short_oid = deleted.oid.to_short_hex();
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
    let outcome = repo
        .branches()
        .rename(&BranchRenameOptions {
            old: old.to_owned(),
            new: new.to_owned(),
            force,
            dry_run,
        })
        .map_err(map_branch_error)?;
    Ok(BranchOutcome::Rename {
        old: outcome.old,
        new: outcome.new,
        oid: outcome.oid.map(|o| o.to_hex()),
        short_oid: outcome.oid.map(|o| o.to_short_hex()),
        replaced: outcome.replaced.map(|o| o.to_hex()),
        dry_run: outcome.dry_run,
    })
}

fn set(repo: &Repository, branch: &str, upstream: &str, dry_run: bool) -> Result<BranchOutcome> {
    let display = repo
        .branches()
        .validate_set_upstream(branch, upstream)
        .map_err(map_branch_error)?;
    if !dry_run {
        repo.branches()
            .set_upstream(branch, upstream)
            .map_err(map_branch_error)?;
    }
    Ok(BranchOutcome::SetUpstream {
        branch: branch.to_owned(),
        upstream: display,
        dry_run,
    })
}

fn unset(repo: &Repository, branch: &str, dry_run: bool) -> Result<BranchOutcome> {
    if dry_run {
        repo.branches()
            .preview_unset_upstream(branch)
            .map_err(map_branch_error)?;
    } else {
        repo.branches()
            .unset_upstream(branch)
            .map_err(map_branch_error)?;
    }
    Ok(BranchOutcome::UnsetUpstream {
        branch: branch.to_owned(),
        dry_run,
    })
}

pub use crate::error_map::map_branch_error;

/// Dispatch parsed CLI args for `Branch`.
pub fn dispatch(
    args: crate::args::BranchArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let BranchArgs {
        name,
        start,
        force,
        track,
        no_track,
        set_upstream_to,
        unset_upstream,
        rename,
        force_rename,
        delete,
        force_delete,
        list,
        dry_run,
    } = args;
    emit_success(
        commands::branch::run(commands::branch::BranchRunOptions {
            name,
            start,
            force,
            track,
            no_track,
            set_upstream_to,
            unset_upstream,
            rename,
            force_rename,
            delete,
            force_delete,
            list,
            list_full_detail: env.branch_list_full_detail(),
            dry_run,
        })?,
        opts,
        ctx,
    )
}

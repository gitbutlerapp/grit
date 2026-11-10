//! `grit restore` — restore working tree and/or index paths.

use grit_lib::progress::{CancelToken, NullProgress};

use anyhow::{Context, Result};
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError, RestoreHunksOutcome};
use grit_lib::porcelain::restore::{ConflictSide, RestoreOptions, RestoreOutcome, RestoreSource};
use grit_lib::revwalk::ResolveOptions;

use crate::conflict_style::{self, ConflictStyleArg};
use std::collections::BTreeMap;

use crate::context;
use crate::pathspec_args::PathspecArgs;
use crate::ui::PathDisplayOptions;

pub use crate::render::restore::RestoreCliOutcome;

impl From<RestoreOutcome> for RestoreCliOutcome {
    fn from(value: RestoreOutcome) -> Self {
        Self {
            restored: value.restored,
            removed: value.removed,
            restored_hunks: BTreeMap::new(),
            restored_lines: BTreeMap::new(),
            dry_run: false,
            path_display: None,
        }
    }
}

impl From<RestoreHunksOutcome> for RestoreCliOutcome {
    fn from(value: RestoreHunksOutcome) -> Self {
        let restored_hunks: BTreeMap<String, Vec<String>> = value
            .restored_hunks
            .into_iter()
            .map(|(path, ids)| (path, ids.into_iter().map(|id| id.as_str()).collect()))
            .collect();
        Self {
            restored: value.restored,
            removed: value.removed,
            restored_hunks,
            restored_lines: value.restored_lines,
            dry_run: value.dry_run,
            path_display: None,
        }
    }
}

/// Exit code for `--hunks` / `--lines` failures.
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some() {
        2
    } else {
        1
    }
}

/// Run partial restore via hunk ids and/or line ranges.
pub fn run_hunks(
    args: &PathspecArgs,
    hunks: &[String],
    lines: &[String],
    staged: bool,
    worktree: bool,
    dry_run: bool,
) -> Result<RestoreCliOutcome> {
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    let mut selection = parse_hunk_id_list(hunks).map_err(super::add::map_hunk_error)?;
    selection.ranges = super::add::parse_line_range_list(lines)?;
    let outcome = repo
        .index()
        .restore_hunks(&selection, &pathspecs, staged, worktree, dry_run)
        .map_err(super::add::map_hunk_error)?;
    Ok(RestoreCliOutcome::from(outcome)
        .with_path_display(PathDisplayOptions::from_repo(&repo).ok()))
}

/// Run `grit restore` with the given flags and path arguments.
pub fn run(
    args: &PathspecArgs,
    staged: bool,
    worktree: bool,
    source: Option<String>,
    conflict_side: Option<ConflictSide>,
    merge: bool,
    conflict_style: Option<ConflictStyleArg>,
) -> Result<RestoreCliOutcome> {
    args.require_non_empty()?;
    let repo = context::discover()?;
    let remerge = if merge || conflict_style.is_some() {
        Some(
            conflict_style
                .map(Into::into)
                .unwrap_or_else(|| conflict_style::conflict_style_from_repo(&repo)),
        )
    } else {
        None
    };
    let resolved = args.resolve(&repo)?;

    let restore_source = match source {
        None => RestoreSource::Index,
        Some(rev) => {
            let oid = repo
                .revwalk()
                .resolve(&rev, &ResolveOptions::default())
                .with_context(|| format!("could not resolve '{rev}'"))?;
            let obj = repo.odb().read(&oid).context("reading source object")?;
            use grit_lib::objects::{parse_commit, ObjectKind};
            let tree = match obj.kind {
                ObjectKind::Commit => parse_commit(&obj.data)?.tree,
                ObjectKind::Tree => oid,
                ObjectKind::Blob | ObjectKind::Tag => {
                    anyhow::bail!("source must be a commit or tree");
                }
            };
            RestoreSource::Tree(tree)
        }
    };

    let pathspecs = resolved.specs().to_vec();
    let pathspec_sources: Vec<String> = resolved.iter().map(str::to_owned).collect();
    let pathspec_sources = if pathspec_sources.is_empty() {
        pathspecs.clone()
    } else {
        pathspec_sources
    };

    let opts = RestoreOptions {
        pathspecs,
        pathspec_sources,
        source: restore_source,
        staged,
        worktree,
        stage: conflict_side,
        remerge,
    };

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
    let outcome = repo
        .checkout()
        .restore(&opts, &mut grit_lib::progress::NullProgress, &cancel)
        .map_err(crate::json_error::map_lib_error)?;
    Ok(RestoreCliOutcome::from(outcome)
        .with_path_display(PathDisplayOptions::from_repo(&repo).ok()))
}

/// Dispatch parsed CLI args for `Restore`.
pub fn dispatch(
    args: crate::args::RestoreArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, finish_hunk_error, CommandEnv};

    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let RestoreArgs {
        pathspecs,
        staged,
        worktree,
        source,
        ours,
        theirs,
        merge,
        conflict_style,
        hunks,
        lines,
        dry_run,
    } = args;
    {
        use grit_lib::porcelain::restore::ConflictSide;
        let side = if ours {
            Some(ConflictSide::Ours)
        } else if theirs {
            Some(ConflictSide::Theirs)
        } else {
            None
        };
        if !hunks.is_empty() || !lines.is_empty() {
            if source.is_some() || side.is_some() || merge || conflict_style.is_some() {
                bail!("--hunks and --lines cannot be used with --source, --ours, --theirs, or --merge");
            }
            match commands::restore::run_hunks(
                &pathspecs, &hunks, &lines, staged, worktree, dry_run,
            ) {
                Ok(outcome) => emit_success(outcome, opts, ctx),
                Err(err) => {
                    let code = commands::restore::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        } else {
            if dry_run {
                bail!("--dry-run requires --hunks or --lines");
            }
            emit_success(
                commands::restore::run(
                    &pathspecs,
                    staged,
                    worktree,
                    source,
                    side,
                    merge,
                    conflict_style,
                )?,
                opts,
                ctx,
            )
        }
    }
}

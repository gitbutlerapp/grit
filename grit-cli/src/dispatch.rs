//! Route a parsed [`crate::args::Command`] to command implementations.


use anyhow::{bail, Result};

use crate::args::*;
use crate::commands;
use crate::context;
use crate::exit_status::ExitStatus;
use crate::json_error;
use crate::message_input;
use crate::output::{self, emit, OutputMode, OutputOptions, RenderContext};
use crate::pager;
use crate::patch_selection;
use crate::progress;
use crate::quiet_args;
use crate::stdio;

/// Invocation context shared by every command `dispatch`.
pub struct CommandEnv<'a> {
    pub opts: &'a OutputOptions,
    pub ctx: &'a RenderContext,
    pub process_env: &'a grit_lib::config::environment::Environment,
}

/// Commands that write objects and so may start automatic maintenance when they succeed.
pub(crate) fn writes_objects(command: &Command) -> bool {
    match command {
        Command::Commit { dry_run, .. } => !dry_run,
        Command::Merge { check, dry_run, .. }
        | Command::Pick { check, dry_run, .. }
        | Command::Revert { check, dry_run, .. } => !check && !dry_run,
        Command::Fetch { fetch } => !fetch.dry_run,
        Command::Pull { fetch, check, .. } => !fetch.dry_run && !check,
        Command::Rebase { shared, .. } => !shared.dry_run,
        Command::History { action } => !history_action_shared(action).dry_run,
        Command::Stash {
            dry_run, action, ..
        } => {
            !dry_run
                && matches!(
                    action,
                    None | Some(StashAction::Push { dry_run: false, .. })
                )
        }
        _ => false,
    }
}

/// Commands that may run credential helpers or remote transports. Those children can exit
/// without reading what grit writes to them, which must fail the write (as in Git) instead
/// of killing grit with `SIGPIPE`.
pub(crate) fn talks_to_remotes(command: &Command) -> bool {
    matches!(
        command,
        Command::Push { .. }
            | Command::Fetch { .. }
            | Command::Pull { .. }
            | Command::Clone { .. }
            | Command::Remote { .. }
            | Command::Auth { .. }
            | Command::Checkout { .. }
            | Command::Checkin { .. }
            | Command::Integrate { .. }
            | Command::Sync
            | Command::Share { .. }
            | Command::Slice { .. }
            | Command::Serve { .. }
            | Command::ReceivePack { .. }
            | Command::UploadPack { .. }
    )
}

fn history_action_shared(action: &HistoryAction) -> &commands::history::HistoryRewriteArgs {
    match action {
        HistoryAction::Drop { shared, .. }
        | HistoryAction::Move { shared, .. }
        | HistoryAction::Reword { shared, .. }
        | HistoryAction::Fixup { shared, .. }
        | HistoryAction::Absorb { shared, .. }
        | HistoryAction::Rebase { shared, .. }
        | HistoryAction::Split { shared, .. }
        | HistoryAction::Undo { shared }
        | HistoryAction::Squash { shared, .. } => shared,
    }
}

pub fn validate_cli(cli: &Cli) -> Result<(), clap::Error> {
    if let Some(Command::Blame { copy_detection, .. }) = &cli.command {
        if *copy_detection > 3 {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                "blame copy detection: at most three -C flags are supported",
            ));
        }
    }
    Ok(())
}

/// Run the selected subcommand and render its outcome.
///
/// Each command computes a typed, serializable outcome; [`emit`] renders it as
/// human text or a single JSON object. The exceptions are `completions` (a raw
/// completion script on stdout), `manager` (a raw credential-helper protocol on
/// stdin/stdout — no outcome), `upload-pack` and `receive-pack` (the Git wire
/// protocol on stdin/stdout — no outcome), and
/// `push` (which emits its per-ref outcome and then exits non-zero when a ref
/// was rejected).
/// Plumbing commands whose stdout is not a command outcome object.
pub(crate) fn reject_non_human_plumbing(cli: &Cli, opts: &OutputOptions) -> Result<()> {
    if opts.mode == OutputMode::Human {
        return Ok(());
    }
    let Some(cmd) = &cli.command else {
        return Ok(());
    };
    let name = match cmd {
        Command::Completions { .. } => "completions",
        Command::Manager { .. } => "manager",
        Command::UploadPack { .. } => "upload-pack",
        Command::ReceivePack { .. } => "receive-pack",
        _ => return Ok(()),
    };
    bail!("`grit {name}` does not support --json or --markdown");
}

/// Whether `err` comes from writing to a reader that has gone away (a quit pager).
fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

fn emit_success<T: output::HumanRender + output::MarkdownRender + serde::Serialize>(
    value: T,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    emit(value, opts, ctx)?;
    Ok(ExitStatus::Success)
}

/// Like [`emit_success`], but renders nothing in human mode under `--quiet`.
fn emit_success_quiet<T: output::HumanRender + output::MarkdownRender + serde::Serialize>(
    value: T,
    quiet: &quiet_args::QuietArgs,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    if quiet.shows_outcome(opts.mode) {
        emit(value, opts, ctx)?;
    }
    Ok(ExitStatus::Success)
}

fn finish_hunk_error(err: anyhow::Error, opts: &OutputOptions, code: i32) -> ExitStatus {
    let err = json_error::ensure_structured(err);
    if opts.mode == OutputMode::Human {
        eprintln!("error: {err:#}");
    } else {
        output::emit_error(&err, opts);
    }
    ExitStatus::from_i32(code)
}

fn emit_rebase_outcome(
    outcome: commands::history::RebaseRunOutcome,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    let status = ExitStatus::from_rebase_run(&outcome);
    emit(outcome, opts, ctx)?;
    Ok(status)
}

pub fn dispatch(cli: crate::args::Cli, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv {
        opts,
        ctx,
        process_env,
    } = env;
    match cli.command.unwrap_or(Command::Status {
        status: commands::status::StatusCliArgs::default(),
    }) {
        Command::Init {
            path,
            bare,
            ref_format,
            initial_branch,
            separate_git_dir,
            object_format,
        } => emit_success(
            commands::init::run(
                path,
                bare,
                ref_format,
                initial_branch,
                separate_git_dir,
                object_format,
            )?,
            opts,
            ctx,
        ),
        // `grit checkout <branch>` is the Git habit for `grit switch`; only a URL
        // asks for a snapshot checkout.
        Command::Checkout {
            url,
            slice: None,
            paths,
            into: None,
        } if paths.is_empty() && !url.contains("://") => emit_success(
            commands::switch::run(Some(url), None, false, false, false, false, false)?,
            opts,
            ctx,
        ),
        Command::Checkout {
            url,
            slice,
            paths,
            into,
        } => emit_success(
            commands::checkout::run(&url, slice.as_deref(), &paths, into.as_deref(), opts.mode)?,
            opts,
            ctx,
        ),
        Command::Checkin { branches, force } => {
            let outcome = commands::checkin::run(&branches, force, opts.mode)?;
            let status = ExitStatus::from_push_rejected(outcome.refused);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Integrate { branch, without } => {
            let outcome = commands::checkin::integrate(&branch, &without, opts.mode)?;
            let status = ExitStatus::from_integrate(&outcome);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Sync => {
            let outcome = commands::sync::run(opts.mode)?;
            let status =
                ExitStatus::from_shared(outcome.needs_attention(), outcome.needs_decision());
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Share { action } => match action {
            ShareAction::Add {
                branch,
                users,
                link,
                uses,
                expires,
                no_history,
                paths,
            } => emit_success(
                commands::share::add(
                    &branch,
                    &users,
                    link,
                    uses.as_deref(),
                    expires,
                    (no_history || !paths.is_empty()).then(|| api_view(paths)),
                    opts.mode,
                )?,
                opts,
                ctx,
            ),
            ShareAction::List { branch } => emit_success(
                commands::share::list(branch.as_deref(), opts.mode)?,
                opts,
                ctx,
            ),
            ShareAction::Accept { link } => {
                emit_success(commands::share::accept(&link, opts.mode)?, opts, ctx)
            }
            ShareAction::Remove { branch, users } => emit_success(
                commands::share::remove(&branch, &users, opts.mode)?,
                opts,
                ctx,
            ),
            ShareAction::Leave { branch } => {
                emit_success(commands::share::leave(&branch, opts.mode)?, opts, ctx)
            }
            ShareAction::Resolve {
                branch,
                option,
                name,
                with,
            } => {
                let outcome = commands::share::resolve(
                    &branch,
                    option.as_deref(),
                    name.as_deref(),
                    with.as_deref(),
                    opts.mode,
                )?;
                let status = ExitStatus::from_shared(false, outcome.needs_decision());
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
            ShareAction::Revoke { branch, link } => {
                emit_success(commands::share::revoke(&branch, &link)?, opts, ctx)
            }
        },
        Command::Slice { action, url } => match action {
            SliceAction::List => emit_success(commands::slice::list(url.as_deref())?, opts, ctx),
            SliceAction::New {
                name,
                paths,
                description,
            } => emit_success(
                commands::slice::new(url.as_deref(), &name, &paths, description.as_deref())?,
                opts,
                ctx,
            ),
            SliceAction::Edit {
                name,
                add,
                remove,
                description,
            } => emit_success(
                commands::slice::edit(
                    url.as_deref(),
                    &name,
                    &add,
                    &remove,
                    description.as_deref(),
                )?,
                opts,
                ctx,
            ),
            SliceAction::Delete { name } => {
                emit_success(commands::slice::delete(url.as_deref(), &name)?, opts, ctx)
            }
        },
        Command::Clone {
            url,
            dir,
            depth,
            branch,
            shallow,
            single_branch,
            no_single_branch,
            no_tags,
            origin,
            blobs,
            config,
            bare,
            no_checkout,
            quiet,
        } => {
            let single_branch = if single_branch {
                Some(true)
            } else if no_single_branch {
                Some(false)
            } else {
                None
            };
            let show_outcome = quiet.shows_outcome(opts.mode);
            let outcome = commands::clone::run(commands::clone::CloneRunOptions {
                url,
                dir,
                depth,
                branch,
                shallow,
                single_branch,
                no_tags,
                origin,
                blobs,
                config,
                bare,
                no_checkout,
                quiet,
                mode: opts.mode,
            })?;
            if show_outcome {
                emit_success(outcome, opts, ctx)?;
            }
            Ok(ExitStatus::Success)
        }
        Command::Remote { verbose, action } => match action {
            None => emit_success(commands::remote::run_list_or_add(None, verbose)?, opts, ctx),
            Some(RemoteAction::Add { name, url, fetch }) => emit_success(
                commands::remote::run_list_or_add(Some((name, url, fetch)), false)?,
                opts,
                ctx,
            ),
            Some(RemoteAction::SetUrl {
                name,
                url,
                push,
                dry_run,
            }) => emit_success(
                commands::remote::run_set_url(&name, &url, push, dry_run)?,
                opts,
                ctx,
            ),
            Some(RemoteAction::Remove { name, dry_run }) => {
                emit_success(commands::remote::run_remove(&name, dry_run)?, opts, ctx)
            }
            Some(RemoteAction::Prune { name, dry_run }) => {
                emit_success(commands::remote::run_prune(&name, dry_run)?, opts, ctx)
            }
            Some(RemoteAction::SetHead {
                name,
                branch,
                auto,
                delete,
                dry_run,
            }) => {
                use grit_lib::repo_handles::SetHead;
                let mode = if delete {
                    SetHead::Delete
                } else if auto {
                    SetHead::Auto
                } else if let Some(branch) = branch {
                    SetHead::Branch(branch)
                } else {
                    return Err(anyhow::Error::from(commands::remote::RemoteUsageError)
                        .context("set-head requires --auto, --delete, or a branch name"));
                };
                emit_success(
                    commands::remote::run_set_head(&name, mode, dry_run)?,
                    opts,
                    ctx,
                )
            }
            Some(RemoteAction::Show {
                name,
                no_query,
                limit,
            }) => emit_success(
                commands::remote::run_show(
                    &name,
                    &commands::remote::RemoteShowCliOptions {
                        query: !no_query,
                        limit,
                    },
                )?,
                opts,
                ctx,
            ),
            Some(RemoteAction::Rename { old, new, dry_run }) => emit_success(
                commands::remote::run_rename(&old, &new, dry_run)?,
                opts,
                ctx,
            ),
            Some(RemoteAction::Refs {
                remote_or_url,
                heads,
                tags,
                sort,
                reverse,
                limit,
                prefixes,
            }) => {
                let refs_cli = commands::remote::RemoteRefsCliOptions {
                    heads,
                    tags,
                    sort,
                    reverse,
                    limit,
                    prefixes,
                };
                emit_success(
                    commands::remote::run_refs(&remote_or_url, &refs_cli, opts.mode)?,
                    opts,
                    ctx,
                )
            }
        },
        Command::Log {
            rev,
            pathspecs,
            cursor,
            format,
            oneline,
            decorate,
            group_by,
            count,
            graph,
            graph_ascii,
            graph_unicode,
            no_pager,
        } => {
            if count && group_by.is_none() {
                bail!("`--count` requires `--group-by`");
            }
            // A person at a terminal gets the whole history through the pager; scripts, agents
            // and pipes get pages with a `--cursor` token.
            let terminal = opts.mode == OutputMode::Human
                && std::io::IsTerminal::is_terminal(&std::io::stdout());
            let graph = graph || graph_unicode;
            let display = commands::log::LogDisplayOptions {
                oneline,
                decorate,
                group_by: group_by.map(|g| match g {
                    LogGroupByArg::Author => commands::log::LogGroupBy::Author,
                }),
                group_count_only: count,
                graph,
                graph_ascii: graph_ascii && !graph_unicode,
                line_counts: terminal,
                columns: None,
                no_commits_in_repo: false,
            };
            if format.patch_text && opts.mode != OutputMode::Human {
                bail!("`--patch-text` cannot be used with --json or --markdown");
            }
            if !terminal {
                let plan = if opts.filter.is_none()
                    && commands::log::listing_should_stream_incrementally(&rev, true)
                {
                    commands::log::listing_stream_plan(&rev, &pathspecs, display, &cursor, &format)?
                } else {
                    None
                };
                if let Some(plan) = plan {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    match commands::log::stream_listing(
                        plan, rev, format, display, opts.mode, ctx, &mut lock,
                    ) {
                        Ok(()) => return Ok(ExitStatus::Success),
                        Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
                        Err(e) => return Err(e),
                    }
                }
                let page = commands::log::LogPage::Paged;
                let outcome =
                    commands::log::run(rev, pathspecs, cursor, format, display, opts.mode, page)?;
                return emit_success(outcome, opts, ctx);
            }
            // With no `-n` or `--cursor`, the whole history, walked a batch at a time.
            let view = if cursor.is_none() && rev.max_count().is_none() {
                commands::log::TerminalLog::Stream(Box::new(commands::log::LogStream::start(
                    rev, pathspecs, format, display,
                )?))
            } else {
                let page = commands::log::LogPage::Paged;
                commands::log::TerminalLog::Listing(Box::new(commands::log::run(
                    rev, pathspecs, cursor, format, display, opts.mode, page,
                )?))
            };
            // Start the pager only once the first batch is walked, so errors and Ctrl-C during
            // a slow start behave as usual.
            let mut pager = if no_pager {
                None
            } else {
                pager::Pager::start(context::discover().ok().as_ref())
            };
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            let w: &mut dyn std::io::Write = match pager.as_mut() {
                Some(pager) => pager,
                None => &mut stdout,
            };
            let result = stdio::with_sigpipe_ignored(|| view.render(w, ctx));
            match result {
                Err(e) if !is_broken_pipe(&e) => Err(e),
                _ => Ok(ExitStatus::Success),
            }
        }
        Command::Revs {
            rev,
            pathspecs,
            count,
            left_right,
            cherry_mark,
        } => {
            let mode = commands::revs::RevsMode {
                count,
                left_right,
                cherry_mark,
            };
            if opts.filter.is_none() && commands::revs::listing_can_stream(&rev, &pathspecs, mode)?
            {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                match commands::revs::stream_listing(rev, pathspecs, opts.mode, ctx, &mut lock) {
                    Ok(()) => return Ok(ExitStatus::Success),
                    Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
                    Err(e) => return Err(e),
                }
            }
            emit_success(
                commands::revs::run(rev, pathspecs, mode, opts.mode)?,
                opts,
                ctx,
            )
        }
        Command::Describe { describe } => match commands::describe::run(&describe)? {
            Some(outcome) => emit_success(outcome, opts, ctx),
            None => {
                anyhow::bail!(
                    "cannot describe '{}': no annotated tag is reachable",
                    describe.rev
                );
            }
        },
        Command::MergeBase {
            a,
            b,
            all,
            is_ancestor,
        } => {
            let outcome = commands::merge_base_cmd::run(&a, &b, all, is_ancestor)?;
            let status = if is_ancestor {
                ExitStatus::from_merge_base(&outcome)
            } else {
                ExitStatus::Success
            };
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Reflog {
            reference,
            max_count,
            since,
            after,
            until,
            before,
            grep,
            action,
        } => match action {
            None => emit_success(
                commands::reflog::run_show(
                    reference,
                    commands::reflog::ReflogListOptions {
                        max_count,
                        since,
                        after,
                        until,
                        before,
                        grep,
                    },
                )?,
                opts,
                ctx,
            ),
            Some(ReflogAction::Delete { specs }) => {
                emit_success(commands::reflog::run_delete(specs)?, opts, ctx)
            }
            Some(ReflogAction::Expire {
                all,
                expire,
                expire_unreachable,
                dry_run,
            }) => emit_success(
                commands::reflog::run_expire(all, expire, expire_unreachable, dry_run)?,
                opts,
                ctx,
            ),
            Some(ReflogAction::List { limit }) => {
                emit_success(commands::reflog::run_list_refs(limit)?, opts, ctx)
            }
        },
        Command::Blame {
            rev_pos,
            file,
            rev,
            line_range,
            first_parent,
            ignore_rev,
            ignore_revs_file,
            group,
            ignore_all_space,
            copy_detection,
            move_detection,
            contents,
        } => emit_success(
            commands::blame::run(
                rev_pos,
                file,
                rev,
                line_range,
                first_parent,
                ignore_rev,
                ignore_revs_file,
                group,
                ignore_all_space,
                copy_detection,
                move_detection,
                contents,
                ctx.now,
            )?,
            opts,
            ctx,
        ),
        Command::Diff {
            staged,
            unstaged,
            against,
            no_index,
            exit_code,
            quiet,
            commits,
            pathspecs,
            format,
            line_format,
        } => {
            if format.patch_text && opts.mode != OutputMode::Human {
                bail!("`--patch-text` cannot be used with --json or --markdown");
            }
            let wants_exit_code = exit_code || quiet || no_index;
            let body_delivery = if quiet || format.patch_text {
                commands::diff::DiffBodyDelivery::MetadataOnly
            } else if opts.mode == crate::output::OutputMode::Json {
                commands::diff::DiffBodyDelivery::Collect
            } else {
                commands::diff::DiffBodyDelivery::Stream(opts.mode)
            };
            let run_opts = commands::diff::DiffRunOptions {
                no_index,
                staged,
                unstaged,
                against,
                commits,
                pathspecs,
                format,
                line_format,
                word_level_inline: matches!(opts.mode, crate::output::OutputMode::Human)
                    && commands::diff::diff_word_level_inline_for_output(),
                quiet,
                emit_json_hunk_ids: opts.mode == crate::output::OutputMode::Json,
                body_delivery,
            };
            let run_result = commands::diff::run(run_opts);
            match run_result {
                Ok(outcome) => {
                    let status = if no_index || wants_exit_code {
                        ExitStatus::from_diff_outcome(&outcome)
                    } else {
                        ExitStatus::Success
                    };
                    if !quiet && !format.patch_text {
                        emit_success(outcome, opts, ctx)?;
                    }
                    Ok(status)
                }
                Err(err) => {
                    let err = json_error::ensure_structured(err);
                    output::emit_error(&err, opts);
                    Ok(ExitStatus::diff_command_error(no_index || wants_exit_code))
                }
            }
        }
        Command::Show {
            objects,
            diff_merges,
            pathspecs,
            format,
        } => {
            if format.patch_text && opts.mode != OutputMode::Human {
                bail!("`--patch-text` cannot be used with --json or --markdown");
            }
            let diff_merges =
                grit_lib::diff::DiffMerges::parse(&diff_merges).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown --diff-merges mode '{diff_merges}' (expected first-parent or combined)"
                    )
                })?;
            emit_success(
                commands::show::run(commands::show::ShowRunOptions {
                    objects,
                    pathspecs,
                    format,
                    word_level_inline: matches!(opts.mode, crate::output::OutputMode::Human)
                        && commands::diff::diff_word_level_inline_for_output(),
                    diff_merges,
                    include_refs: matches!(opts.mode, crate::output::OutputMode::Json),
                })?,
                opts,
                ctx,
            )
        }
        Command::Status { status } => emit_success(commands::status::run(&status)?, opts, ctx),
        Command::Shortlog { shortlog } => {
            emit_success(commands::shortlog::run(&shortlog)?, opts, ctx)
        }
        Command::Add {
            pathspecs,
            update,
            dry_run,
            intent_to_add,
            force,
            executable,
            non_executable,
            patch_file,
            hunks,
            lines,
            unified_context,
        } => {
            if unified_context.is_some() && hunks.is_empty() && lines.is_empty() {
                bail!("-U / --unified requires --hunks or --lines");
            }
            if let Some(file) = patch_file {
                if !hunks.is_empty() || !lines.is_empty() {
                    bail!("--patch-file cannot be used with --hunks or --lines");
                }
                if executable || non_executable {
                    bail!("--patch-file cannot be used with --executable or --non-executable");
                }
                if force {
                    bail!("--patch-file cannot be used with --force");
                }
                if update {
                    bail!("--patch-file cannot be used with --update");
                }
                if intent_to_add {
                    bail!("--patch-file cannot be used with --intent-to-add");
                }
                if !pathspecs.paths.is_empty() || pathspecs.pathspec_from_file.is_some() {
                    bail!("--patch-file cannot be used with path arguments");
                }
                emit_success(commands::add::run_patch_file(&file, dry_run)?, opts, ctx)
            } else if !hunks.is_empty() || !lines.is_empty() {
                if executable || non_executable {
                    bail!(
                        "--hunks and --lines cannot be used with --executable or --non-executable"
                    );
                }
                if force {
                    bail!("--hunks and --lines cannot be used with --force");
                }
                if update {
                    bail!("--hunks and --lines cannot be used with --update");
                }
                if intent_to_add {
                    bail!("--hunks and --lines cannot be used with --intent-to-add");
                }
                match commands::add::run_hunks(&pathspecs, &hunks, &lines, unified_context, dry_run)
                {
                    Ok(outcome) => emit_success(outcome, opts, ctx),
                    Err(err) => {
                        let code = commands::add::hunks_exit_code(&err);
                        Ok(finish_hunk_error(err, opts, code))
                    }
                }
            } else {
                emit_success(
                    commands::add::run(
                        &pathspecs,
                        update,
                        dry_run,
                        intent_to_add,
                        force,
                        executable,
                        non_executable,
                        &mut progress::TerminalProgress::new(opts.mode),
                    )?,
                    opts,
                    ctx,
                )
            }
        }
        Command::Rm {
            pathspecs,
            cached,
            force,
            dry_run,
            ignore_unmatch,
        } => emit_success(
            commands::rm::run(&pathspecs, cached, force, dry_run, ignore_unmatch)?,
            opts,
            ctx,
        ),
        Command::Mv {
            paths,
            force,
            dry_run,
        } => emit_success(commands::mv::run(&paths, force, dry_run)?, opts, ctx),
        Command::Clean {
            pathspecs,
            force,
            dry_run,
            ignored,
            only_ignored,
            exclude,
            files_only,
        } => emit_success(
            commands::clean::run(
                &pathspecs,
                force,
                dry_run,
                ignored,
                only_ignored,
                &exclude,
                files_only,
            )?,
            opts,
            ctx,
        ),
        Command::Repack {
            force,
            dry_run,
            quiet,
        } => emit_success(commands::repack::run(force, dry_run, quiet)?, opts, ctx),
        Command::Gc {
            dry_run,
            auto,
            quiet,
            action,
        } => {
            let mode = opts.mode;
            if auto && action.is_some() {
                bail!("`--auto` cannot be combined with a gc subcommand");
            }
            let outcome = match action {
                None if auto => commands::gc::run_auto(dry_run, quiet, ctx.now)?,
                None => commands::gc::run(dry_run, quiet, mode)?,
                Some(GcAction::Repack {
                    bitmaps,
                    force,
                    dry_run: repack_dry,
                }) => commands::gc::run_repack(bitmaps, force, dry_run || repack_dry, quiet, mode)?,
                Some(GcAction::CommitGraph { dry_run: sub_dry }) => {
                    commands::gc::run_commit_graph(dry_run || sub_dry, quiet, mode)?
                }
                Some(GcAction::Midx { dry_run: sub_dry }) => {
                    commands::gc::run_midx(dry_run || sub_dry, quiet, mode)?
                }
                Some(GcAction::PackRefs { dry_run: sub_dry }) => {
                    commands::gc::run_pack_refs(dry_run || sub_dry, quiet, mode)?
                }
                Some(GcAction::Prune {
                    older_than,
                    dry_run: sub_dry,
                }) => {
                    commands::gc::run_prune(older_than.as_deref(), dry_run || sub_dry, quiet, mode)?
                }
                Some(GcAction::Offload {
                    to,
                    blobs,
                    lop_dir,
                    configure_lop,
                    dry_run: sub_dry,
                }) => commands::gc::run_offload(
                    commands::gc::OffloadArgs {
                        remote: to,
                        blobs,
                        lop_dir,
                        configure_lop,
                        dry_run: dry_run || sub_dry,
                    },
                    quiet,
                    mode,
                    ctx.now,
                )?,
                Some(GcAction::DropLargeBlobs {
                    min_size,
                    older_than,
                    dry_run: sub_dry,
                }) => commands::gc::run_drop_large_blobs(
                    min_size.as_deref(),
                    older_than.as_deref(),
                    dry_run || sub_dry,
                    quiet,
                    mode,
                    ctx.now,
                )?,
            };
            emit_success(outcome, opts, ctx)
        }
        Command::Fsck {
            connectivity_only,
            dangling,
            no_dangling,
            strict,
            verbose,
        } => {
            let outcome = commands::fsck::run(commands::fsck::FsckRunOptions {
                connectivity_only,
                dangling,
                no_dangling,
                strict,
                verbose,
                mode: opts.mode,
            })?;
            let status = ExitStatus::from_fsck(&outcome);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Ignore { action } => match action {
            IgnoreAction::Check { paths, all } => {
                let outcome = commands::ignore::run(&paths, all)?;
                let status = ExitStatus::from_ignore_check(&outcome);
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
        },
        Command::Hooks { limit, action } => match action {
            None => emit_success(commands::hooks::run_list(limit.get())?, opts, ctx),
            Some(HooksAction::List { limit: sub_limit }) => emit_success(
                commands::hooks::run_list(sub_limit.get().or(limit.get()))?,
                opts,
                ctx,
            ),
            Some(HooksAction::Run { name, hook_args }) => {
                let outcome = commands::hooks::run_hook_by_name(&name, &hook_args)?;
                let status = ExitStatus::from_hooks(&outcome);
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
        },
        Command::PatchId { revs, unstable } => {
            emit_success(commands::patch_id::run(&revs, unstable)?, opts, ctx)
        }
        Command::Resolve {
            specs,
            short,
            abbrev_ref,
            symbolic_full_name,
            verify_spec,
            quiet,
        } => {
            use commands::resolve::RefNameOutput;
            if let Some(spec) = verify_spec {
                let outcome = commands::resolve::run_verify(spec, short, quiet);
                let code = commands::resolve::verify_exit_code(&outcome);
                if quiet {
                    std::process::exit(code);
                }
                emit_success(outcome?, opts, ctx)
            } else {
                if quiet {
                    anyhow::bail!("--quiet requires --verify");
                }
                let ref_output = if abbrev_ref {
                    RefNameOutput::AbbrevRef
                } else if symbolic_full_name {
                    RefNameOutput::SymbolicFullName
                } else {
                    RefNameOutput::ObjectIds
                };
                emit_success(commands::resolve::run(specs, short, ref_output)?, opts, ctx)
            }
        }
        Command::Cat { spec } => emit_success(commands::cat::run(spec, opts.mode)?, opts, ctx),
        Command::Commit {
            commit_message,
            message_input,
            quiet,
            author,
            author_date,
            trailers,
            signoff,
            reset_author,
            reuse_message,
            gpg_sign,
            no_gpg_sign,
            no_verify,
            all,
            staged,
            amend,
            allow_empty,
            dry_run,
            patch_file,
            hunks,
            lines,
            paths,
        } => {
            let message_flag = message_input::joined_message_from_flags(&message_input.message);
            let message_file = message_input.message_file.clone();
            let author_override = author.author.clone();
            let message = commit_message;
            patch_selection::ensure_single_patch_mode(
                patch_file.is_some(),
                !hunks.is_empty(),
                !lines.is_empty(),
            )?;
            if let Some(file) = patch_file {
                emit_success_quiet(
                    commands::commit::run_patch_file(
                        message.or(message_flag),
                        message_file,
                        author_override,
                        author_date,
                        trailers,
                        signoff,
                        reset_author,
                        reuse_message,
                        gpg_sign,
                        no_gpg_sign,
                        no_verify,
                        dry_run,
                        amend,
                        allow_empty,
                        &file,
                    )?,
                    &quiet,
                    opts,
                    ctx,
                )
            } else if !hunks.is_empty() || !lines.is_empty() {
                match commands::commit::run_hunks(
                    message.or(message_flag),
                    message_file,
                    author_override,
                    author_date,
                    trailers,
                    signoff,
                    reset_author,
                    reuse_message,
                    gpg_sign,
                    no_gpg_sign,
                    no_verify,
                    dry_run,
                    amend,
                    allow_empty,
                    &paths,
                    &hunks,
                    &lines,
                ) {
                    Ok(outcome) => emit_success_quiet(outcome, &quiet, opts, ctx),
                    Err(err) => {
                        let code = commands::commit::hunks_exit_code(&err);
                        Ok(finish_hunk_error(err, opts, code))
                    }
                }
            } else {
                emit_success_quiet(
                    commands::commit::run(
                        message.or(message_flag),
                        message_file,
                        author_override,
                        author_date,
                        trailers,
                        signoff,
                        reset_author,
                        reuse_message,
                        gpg_sign,
                        no_gpg_sign,
                        no_verify,
                        dry_run,
                        amend,
                        allow_empty,
                        all,
                        staged,
                        paths,
                        &mut quiet.progress(opts.mode),
                    )?,
                    &quiet,
                    opts,
                    ctx,
                )
            }
        }
        Command::Branch {
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
        } => emit_success(
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
                list_full_detail: matches!(opts.mode, OutputMode::Json | OutputMode::Markdown),
                dry_run,
            })?,
            opts,
            ctx,
        ),
        Command::Bundle { action } => match action {
            BundleAction::Create {
                file,
                revs,
                all,
                dry_run,
            } => emit_success(
                commands::bundle::run_create(file.into(), revs, all, dry_run)?,
                opts,
                ctx,
            ),
            BundleAction::Verify { file } => {
                let outcome = commands::bundle::run_verify(file.into())?;
                let ok = outcome.ok;
                emit_success(outcome, opts, ctx)?;
                Ok(ExitStatus::from_bundle_verify_ok(ok))
            }
            BundleAction::List { file, limit } => emit_success(
                commands::bundle::run_list(file.into(), limit.get())?,
                opts,
                ctx,
            ),
            BundleAction::Unbundle { file, dry_run } => emit_success(
                commands::bundle::run_unbundle(file.into(), dry_run)?,
                opts,
                ctx,
            ),
        },
        Command::Notes {
            notes_ref,
            dry_run,
            limit,
            action,
        } => {
            use commands::notes::NotesRunAction::*;
            let (run_action, list_limit) = match action {
                None => (List, limit.get()),
                Some(NotesAction::List { limit: sub_limit }) => {
                    (List, sub_limit.get().or(limit.get()))
                }
                Some(NotesAction::Show { rev }) => (Show { rev }, None),
                Some(NotesAction::Add {
                    rev,
                    message,
                    force,
                }) => {
                    let message = message_input::read_required_normalized_message(&message)?;
                    (
                        Add {
                            rev,
                            message,
                            force,
                        },
                        None,
                    )
                }
                Some(NotesAction::Remove { revs }) => (Remove { revs }, None),
            };
            emit_success(
                commands::notes::run(notes_ref, run_action, dry_run, list_limit, ctx.now)?,
                opts,
                ctx,
            )
        }
        Command::Tag {
            name,
            target,
            list,
            sort,
            reverse,
            contains,
            points_at,
            limit,
            delete,
            message,
            annotate,
            legacy_message_file,
            object,
            force,
            dry_run,
            sign,
            no_sign,
            signing_key,
            verify,
            verify_names,
            no_signature,
            tree_sha256,
            exclude_submodule,
        } => {
            let sign_override = if sign {
                Some(true)
            } else if no_sign {
                Some(false)
            } else {
                None
            };
            let message = message_input::merge_tag_legacy_file(message, legacy_message_file);
            let message_flag = message_input::joined_message_from_flags(&message.message);
            let message_file = message.message_file.clone();
            let outcome = commands::tag::run(commands::tag::TagRunOptions {
                name,
                target,
                object,
                delete,
                message: message_flag,
                message_file,
                annotate,
                force,
                dry_run,
                sign: sign_override,
                signing_key,
                now: ctx.now,
                verify,
                verify_names,
                no_signature,
                list,
                sort,
                reverse,
                contains,
                points_at,
                limit: limit.get(),
                tree_sha256,
                exclude_submodule,
                list_detail: if opts.mode == OutputMode::Human {
                    grit_lib::porcelain::tag::TagDetail::Names
                } else {
                    grit_lib::porcelain::tag::TagDetail::Annotation
                },
            })?;
            let status = if verify {
                ExitStatus::from_tag_verify(&outcome)
            } else {
                ExitStatus::Success
            };
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Worktree { action } => match action {
            WorktreeAction::List => emit_success(commands::worktree::run_list()?, opts, ctx),
            WorktreeAction::Add {
                path,
                start_point,
                branch,
                force_branch,
                detach,
                force,
                lock,
                reason,
                no_checkout,
                track,
            } => emit_success(
                commands::worktree::run_add(
                    path,
                    start_point,
                    branch,
                    force_branch,
                    detach,
                    force,
                    lock,
                    reason,
                    no_checkout,
                    track,
                )?,
                opts,
                ctx,
            ),
            WorktreeAction::Remove { path, force } => {
                emit_success(commands::worktree::run_remove(path, force)?, opts, ctx)
            }
            WorktreeAction::Prune { dry_run } => {
                emit_success(commands::worktree::run_prune(dry_run)?, opts, ctx)
            }
            WorktreeAction::Lock { path, reason } => {
                emit_success(commands::worktree::run_lock(path, reason)?, opts, ctx)
            }
            WorktreeAction::Unlock { path } => {
                emit_success(commands::worktree::run_unlock(path)?, opts, ctx)
            }
            WorktreeAction::Move {
                worktree,
                new_path,
                force,
                dry_run,
            } => emit_success(
                commands::worktree::run_move(worktree, new_path, force, dry_run)?,
                opts,
                ctx,
            ),
            WorktreeAction::Repair { paths, dry_run } => {
                emit_success(commands::worktree::run_repair(paths, dry_run)?, opts, ctx)
            }
        },
        Command::Switch {
            branch,
            start_point,
            create,
            track,
            no_track,
            detach,
            dry_run,
        } => {
            let outcome = commands::switch::run(
                branch,
                start_point,
                create,
                detach,
                track,
                no_track,
                dry_run,
            )?;
            commands::clone::emit_path_collision_warning(opts.mode, &outcome.path_collisions);
            emit_success(outcome, opts, ctx)
        }
        Command::Reset {
            rev,
            soft,
            mixed,
            hard,
            keep,
            dry_run,
            orphan_limit,
        } => emit_success(
            commands::reset::run(rev, soft, mixed, hard, keep, dry_run, orphan_limit)?,
            opts,
            ctx,
        ),
        Command::Merge {
            branch,
            abort,
            continue_merge,
            check,
            dry_run,
            ff_only,
            no_ff,
            no_commit,
            squash,
            allow_unrelated_histories,
            no_verify,
            strategy,
            message,
        } => {
            let outcome = commands::merge::run(
                branch.as_deref(),
                abort,
                continue_merge,
                check,
                dry_run,
                commands::merge::MergeRunOptions {
                    ff_only,
                    no_ff,
                    no_commit,
                    squash,
                    allow_unrelated_histories,
                    no_verify,
                    strategy,
                    message,
                },
                &mut progress::TerminalProgress::new(opts.mode),
            )?;
            let status = ExitStatus::from_merge_outcome(&outcome);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Command::Pick {
            commits,
            mainline,
            continue_pick,
            abort,
            skip,
            record_origin,
            no_commit,
            empty,
            message,
            check,
            dry_run,
            replay_merge,
        } => {
            let outcome = commands::pick::run(commands::pick::PickRunOptions {
                commits,
                continue_pick,
                abort,
                skip,
                record_origin,
                no_commit,
                empty,
                message,
                mainline,
                check,
                dry_run,
                replay_merge,
            })?;
            let result = outcome.result;
            emit_success(outcome, opts, ctx)?;
            Ok(ExitStatus::from_pick_result(result))
        }
        Command::Revert {
            commits,
            mainline,
            continue_revert,
            abort,
            skip,
            no_commit,
            empty,
            message,
            check,
            dry_run,
            replay_merge,
        } => {
            let outcome = commands::revert::run(&commands::revert::RevertRunOptions {
                commits,
                mainline,
                continue_revert,
                abort,
                skip,
                no_commit,
                empty,
                message,
                check,
                dry_run,
                replay_merge,
            })?;
            let result = outcome.result;
            emit_success(outcome, opts, ctx)?;
            Ok(ExitStatus::from_revert_result(result))
        }
        Command::History { action } => match action {
            HistoryAction::Drop { commit, shared } => {
                emit_success(commands::history::run_drop(&commit, &shared)?, opts, ctx)
            }
            HistoryAction::Move {
                commit,
                before,
                after,
                shared,
            } => emit_success(
                commands::history::run_move(&commit, before, after, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Reword {
                commit,
                message,
                author,
                shared,
            } => emit_success(
                commands::history::run_reword(&commit, &message, &author, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Fixup {
                commit,
                message,
                hunks,
                lines,
                pathspecs,
                shared,
            } => {
                let outcome = commands::history::run_fixup(
                    &commit, &message, &hunks, &lines, &pathspecs, &shared,
                );
                match outcome {
                    Ok(out) => emit_success(out, opts, ctx),
                    Err(err) => {
                        let code = commands::add::hunks_exit_code(&err);
                        Ok(finish_hunk_error(err, opts, code))
                    }
                }
            }
            HistoryAction::Rebase {
                upstream,
                onto,
                continue_rebase,
                abort,
                skip,
                rebase_flags,
                shared,
            } => {
                let outcome =
                    commands::history::run_rebase(&commands::history::RebaseRunOptions {
                        upstream,
                        onto,
                        continue_rebase,
                        abort,
                        skip,
                        flags: &rebase_flags,
                        shared: &shared,
                    })?;
                emit_rebase_outcome(outcome, opts, ctx)
            }
            HistoryAction::Absorb { base, shared } => emit_success(
                commands::history::run_absorb(base.as_deref(), &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Undo { shared } => {
                emit_success(commands::history::run_undo(&shared)?, opts, ctx)
            }
            HistoryAction::Squash {
                range,
                message,
                combine,
                shared,
            } => emit_success(
                commands::history::run_squash(&range, &message, combine, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Split {
                commit,
                message,
                rest_message,
                paths,
                shared,
            } => emit_success(
                commands::history::run_split(&commit, &message, rest_message, &paths, &shared)?,
                opts,
                ctx,
            ),
        },
        Command::Stash {
            action,
            message,
            include_untracked,
            hunks,
            lines,
            dry_run,
            staged,
            keep_index,
            stash_limit,
            pathspecs,
        } => match action {
            None => {
                let outcome = commands::stash::run_push(commands::stash::StashPushRunArgs {
                    message,
                    include_untracked,
                    staged,
                    keep_index,
                    hunks: &hunks,
                    lines: &lines,
                    dry_run,
                    limit: stash_limit,
                    pathspec_args: &pathspecs,
                    progress: &mut progress::TerminalProgress::new(opts.mode),
                });
                match outcome {
                    Ok(o) => emit_success(o, opts, ctx),
                    Err(err) => {
                        let code = commands::stash::hunks_exit_code(&err);
                        Ok(finish_hunk_error(err, opts, code))
                    }
                }
            }
            Some(StashAction::Push {
                message: push_message,
                include_untracked: push_untracked,
                hunks: push_hunks,
                lines: push_lines,
                dry_run: push_dry_run,
                staged: push_staged,
                keep_index: push_keep_index,
                pathspecs: push_pathspecs,
            }) => {
                let merged_hunks = if push_hunks.is_empty() {
                    hunks
                } else {
                    push_hunks
                };
                let merged_lines = if push_lines.is_empty() {
                    lines
                } else {
                    push_lines
                };
                let merged_pathspecs = if push_pathspecs.paths.is_empty()
                    && push_pathspecs.pathspec_from_file.is_none()
                {
                    pathspecs
                } else {
                    push_pathspecs
                };
                let outcome = commands::stash::run_push(commands::stash::StashPushRunArgs {
                    message: push_message.or(message),
                    include_untracked: include_untracked || push_untracked,
                    staged: staged || push_staged,
                    keep_index: keep_index || push_keep_index,
                    hunks: &merged_hunks,
                    lines: &merged_lines,
                    dry_run: dry_run || push_dry_run,
                    limit: stash_limit,
                    pathspec_args: &merged_pathspecs,
                    progress: &mut progress::TerminalProgress::new(opts.mode),
                });
                match outcome {
                    Ok(o) => emit_success(o, opts, ctx),
                    Err(err) => {
                        let code = commands::stash::hunks_exit_code(&err);
                        Ok(finish_hunk_error(err, opts, code))
                    }
                }
            }
            Some(StashAction::List) => emit_success(commands::stash::run_list()?, opts, ctx),
            Some(StashAction::Show {
                index,
                stat: _,
                patch,
                include_untracked,
            }) => emit_success(
                commands::stash::run_show(index, patch, include_untracked)?,
                opts,
                ctx,
            ),
            Some(StashAction::Branch { name, index }) => {
                let outcome = commands::stash::run_branch(index, &name)?;
                let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
            Some(StashAction::Clear { force, dry_run }) => {
                let outcome = commands::stash::run_clear(force, dry_run)?;
                emit_success(outcome, opts, ctx)?;
                if !force {
                    Ok(ExitStatus::Failure)
                } else {
                    Ok(ExitStatus::Success)
                }
            }
            Some(StashAction::Apply {
                index,
                restore_index,
            }) => {
                let outcome = commands::stash::run_apply(
                    index,
                    restore_index,
                    &mut progress::TerminalProgress::new(opts.mode),
                )?;
                let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
            Some(StashAction::Pop {
                index,
                restore_index,
            }) => {
                let outcome = commands::stash::run_pop(
                    index,
                    restore_index,
                    &mut progress::TerminalProgress::new(opts.mode),
                )?;
                let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
                emit_success(outcome, opts, ctx)?;
                Ok(status)
            }
            Some(StashAction::Drop { index }) => {
                emit_success(commands::stash::run_drop(index)?, opts, ctx)
            }
        },
        Command::Restore {
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
        } => {
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
        Command::Fetch { fetch } => {
            let outcome = commands::fetch::run(&fetch, opts.mode)?;
            let status = ExitStatus::from_fetch(&outcome);
            if fetch.quiet.shows_outcome(opts.mode) {
                match outcome {
                    commands::fetch::FetchRunOutcome::One(one) => {
                        emit_success(one, opts, ctx)?;
                    }
                    commands::fetch::FetchRunOutcome::All(all) => {
                        emit_success(all, opts, ctx)?;
                    }
                }
            }
            Ok(status)
        }
        Command::Pull {
            fetch,
            ff_only,
            no_rebase,
            rebase,
            autostash,
            check,
        } => {
            let args = commands::pull::PullRunArgs {
                fetch,
                ff_only,
                no_rebase,
                rebase,
                autostash,
                check,
            };
            let outcome = commands::pull::run(&args, opts.mode)?;
            let status = ExitStatus::from_pull(&outcome);
            if args.fetch.quiet.shows_outcome(opts.mode) {
                emit_success(outcome, opts, ctx)?;
            }
            Ok(status)
        }
        Command::Rebase {
            upstream,
            onto,
            continue_rebase,
            abort,
            skip,
            rebase_flags,
            shared,
        } => {
            let outcome = commands::history::run_rebase(&commands::history::RebaseRunOptions {
                upstream,
                onto,
                continue_rebase,
                abort,
                skip,
                flags: &rebase_flags,
                shared: &shared,
            })?;
            emit_rebase_outcome(outcome, opts, ctx)
        }
        Command::Push {
            tags,
            delete,
            all,
            mirror,
            prune,
            remote,
            dry_run,
            force,
            force_with_lease,
            set_upstream,
            atomic,
            push_option,
            follow_tags,
            quiet,
            no_verify,
            remote_name,
            refspecs,
        } => {
            let mut positionals = Vec::new();
            if let Some(r) = remote_name {
                positionals.push(r);
            }
            positionals.extend(refspecs);
            let outcome = commands::push::run(
                tags,
                delete,
                all,
                mirror,
                prune,
                remote,
                &positionals,
                dry_run,
                force,
                force_with_lease,
                set_upstream,
                atomic,
                &push_option,
                follow_tags,
                &quiet,
                opts.mode,
                no_verify,
            )?;
            let rejected = outcome.rejected;
            let decide = outcome.needs_decision();
            if rejected || decide || quiet.shows_outcome(opts.mode) {
                emit_success(outcome, opts, ctx)?;
            }
            // The outcome (per-ref results) is reported in both modes; a rejected
            // push is still a failure, so mirror Git and exit non-zero. A shared
            // branch whose heads need a decision exits 3.
            Ok(ExitStatus::from_shared(rejected, decide))
        }
        Command::Auth {
            url,
            token,
            username,
            action,
        } => match action {
            None => {
                let outcome = if let (Some(url), Some(token), Some(username)) =
                    (url.as_deref(), token.as_deref(), username.as_deref())
                {
                    commands::auth::store_token(url, username, token)?
                } else {
                    let server = url
                        .as_deref()
                        .map(commands::auth::grit_server)
                        .transpose()?;
                    match (server.flatten(), token) {
                        (Some(server), Some(token)) => {
                            commands::auth::run_server_token(&server, &token)?
                        }
                        (Some(server), None) => commands::auth::run_server_login(&server)?,
                        (None, Some(_)) if url.is_some() => {
                            anyhow::bail!("--token is for `grit serve` hosts; GitHub signs in with the browser, or store a personal access token with `--username <user> --token <token>`")
                        }
                        (None, token) => match (commands::auth::default_host()?, token) {
                            (commands::auth::DefaultHost::Grit(server), Some(token)) => {
                                commands::auth::run_server_token(&server, &token)?
                            }
                            (commands::auth::DefaultHost::Grit(server), None) => {
                                commands::auth::run_server_login(&server)?
                            }
                            (commands::auth::DefaultHost::GitHub, Some(_)) => {
                                anyhow::bail!("--token is for `grit serve` hosts; GitHub signs in with the browser, or store a personal access token with `--username <user> --token <token>`")
                            }
                            (commands::auth::DefaultHost::GitHub, None) => commands::auth::run()?,
                        },
                    }
                };
                emit_success(outcome, opts, ctx)
            }
            Some(AuthAction::Logout { url, dry_run }) => {
                let server = match url.as_deref() {
                    Some(url) => commands::auth::grit_server(url)?,
                    None => match commands::auth::default_host()? {
                        commands::auth::DefaultHost::Grit(server) => Some(server),
                        commands::auth::DefaultHost::GitHub => None,
                    },
                };
                emit_success(commands::auth::logout(server.as_ref(), dry_run)?, opts, ctx)
            }
        },
        Command::Update => emit_success(commands::update::run(opts.mode)?, opts, ctx),
        Command::Skill => emit_success(commands::skill::run()?, opts, ctx),
        Command::Completions { shell } => {
            commands::completions::run(shell)?;
            Ok(ExitStatus::Success)
        }
        Command::Config {
            global,
            local,
            system,
            worktree,
            file,
            list,
            all,
            add,
            unset,
            unset_all,
            rename_section,
            rename_section_new,
            remove_section,
            value_type,
            default_value,
            dry_run,
            key,
            value,
        } => {
            let rename_pair = match (rename_section, rename_section_new) {
                (Some(old), Some(new)) => Some((old, new)),
                (None, None) => None,
                _ => {
                    bail!("--rename-section requires both OLD and --rename-section-new NEW");
                }
            };
            let outcome = commands::config::run(
                process_env,
                global,
                local,
                system,
                worktree,
                file,
                list,
                unset,
                unset_all,
                add,
                all,
                value_type,
                default_value,
                rename_pair,
                remove_section,
                dry_run,
                key,
                value,
                opts.mode,
            )?;
            let code = commands::config::exit_code(&outcome);
            emit(outcome, opts, ctx)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(ExitStatus::Success)
        }
        // `manager` speaks Git's credential protocol on stdout; it has no JSON form.
        Command::Manager { operation } => {
            commands::manager::run(&operation)?;
            Ok(ExitStatus::Success)
        }
        Command::Serve {
            action,
            root,
            db,
            bind,
            setup,
            email,
            bundle_interval,
            no_bundles,
            header_timeout,
            idle_timeout,
            max_git_requests,
            shutdown_timeout,
        } => {
            let db = db.as_deref();
            match action {
                Some(ServeAction::AddUser { email, admin }) => {
                    let password = commands::serve_http::read_new_password(&email)?;
                    emit_success(
                        commands::serve_http::add_user(&root, db, &email, admin, password)?,
                        opts,
                        ctx,
                    )
                }
                Some(ServeAction::Token { email }) => {
                    emit_success(commands::serve_http::token(&root, db, &email)?, opts, ctx)
                }
                Some(ServeAction::Grant {
                    user,
                    repository,
                    write,
                }) => emit_success(
                    commands::serve_http::grant(&root, db, &user, &repository, write)?,
                    opts,
                    ctx,
                ),
                Some(ServeAction::Users) => {
                    emit_success(commands::serve_http::users(&root, db)?, opts, ctx)
                }
                None if setup => {
                    let email = commands::serve_http::read_admin_email(email)?;
                    let password = commands::serve_http::read_new_password(&email)?;
                    emit_success(
                        commands::serve_http::setup(&root, db, &email, password)?,
                        opts,
                        ctx,
                    )
                }
                // The running server logs to stderr; it has no stdout result.
                None => {
                    let bundles = grit_http_server::bundles::BundleSettings {
                        enabled: !no_bundles,
                        interval: bundle_interval,
                        ..Default::default()
                    };
                    let limits = grit_http_server::server::ServeLimits {
                        header_timeout,
                        idle_timeout,
                        max_git_requests,
                    };
                    commands::serve_http::run(
                        &root,
                        db,
                        &bind,
                        bundles,
                        limits,
                        grit_http_server::server::Shutdown::grace(shutdown_timeout),
                    )?;
                    Ok(ExitStatus::Success)
                }
            }
        }
        // The server programs speak the Git wire protocol on stdout.
        Command::UploadPack { args } => {
            commands::serve::run(
                commands::serve::Service::UploadPack,
                &args.directory,
                args.stateless_rpc,
                args.advertise_refs,
            )?;
            Ok(ExitStatus::Success)
        }
        Command::ReceivePack { args } => {
            commands::serve::run(
                commands::serve::Service::ReceivePack,
                &args.directory,
                args.stateless_rpc,
                args.advertise_refs,
            )?;
            Ok(ExitStatus::Success)
        }
    }
}

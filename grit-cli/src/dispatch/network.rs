//! Dispatch for the network commands: fetch, pull, push, clone, remote and bundle.

use anyhow::Result;

use crate::args::{
    BundleAction, BundleArgs, CloneArgs, FetchArgs, PullArgs, PushArgs, RemoteAction, RemoteArgs,
};
use crate::commands;
use crate::dispatch::{emit_success, CommandEnv};
use crate::exit_status::ExitStatus;

/// Dispatch parsed CLI args for `Fetch`.
pub fn fetch(args: FetchArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let outcome = commands::fetch::run(&args, opts.mode.is_human())?;
    let status = ExitStatus::from_fetch(&outcome);
    if args.quiet.shows_outcome(opts.mode) {
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

/// Dispatch parsed CLI args for `Pull`.
pub fn pull(args: PullArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let PullArgs {
        fetch,
        ff_only,
        no_rebase,
        rebase,
        autostash,
        check,
    } = args;
    let args = commands::pull::PullRunArgs {
        fetch,
        ff_only,
        no_rebase,
        rebase,
        autostash,
        check,
    };
    let outcome = commands::pull::run(&args, opts.mode.is_human())?;
    let status = ExitStatus::from_pull(&outcome);
    if args.fetch.quiet.shows_outcome(opts.mode) {
        emit_success(outcome, opts, ctx)?;
    }
    Ok(status)
}

/// Dispatch parsed CLI args for `Push`.
pub fn push(args: PushArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let PushArgs {
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
        no_verify,
        quiet,
        remote_name,
        refspecs,
    } = args;
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
        opts.mode.is_human(),
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

/// Dispatch parsed CLI args for `Clone`.
pub fn clone(args: CloneArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let CloneArgs {
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
    } = args;
    let single_branch = if single_branch {
        Some(true)
    } else if no_single_branch {
        Some(false)
    } else {
        None
    };
    let show_outcome = quiet.shows_outcome(opts.mode);
    let outcome = commands::clone::run(
        commands::clone::CloneRunOptions {
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
        },
        opts.mode.is_human(),
    )?;
    outcome.render_notes(&mut std::io::stderr(), opts.mode)?;
    if show_outcome {
        emit_success(outcome, opts, ctx)?;
    }
    Ok(ExitStatus::Success)
}

/// Dispatch parsed CLI args for `Remote`.
pub fn remote(args: RemoteArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let RemoteArgs { verbose, action } = args;
    match action {
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
                commands::remote::run_refs(&remote_or_url, &refs_cli)?,
                opts,
                ctx,
            )
        }
    }
}

/// Dispatch parsed CLI args for `Bundle`.
pub fn bundle(args: BundleArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let BundleArgs { action } = args;
    match action {
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
    }
}

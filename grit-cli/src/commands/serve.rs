//! `grit upload-pack` and `grit receive-pack` — the server side of fetch and push.
//!
//! These are plumbing commands. You don't run them by hand: a Git client starts
//! them on the remote end of an ssh connection, and `grit-http-server` runs them
//! to answer smart HTTP requests. They speak the Git wire protocol on stdin and
//! stdout, so they produce no human or JSON output of their own. Pushes get the
//! same guard as `grit serve` ([`grit_protocol::PushGuard`]).

use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::stdio;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error as LibError;
use grit_lib::remote::serve::{self, ProtocolVersion, ReceivePolicy, ServeOptions};
use grit_lib::repo::Repository;

/// Which server program to run.
#[derive(Debug, Clone, Copy)]
pub enum Service {
    /// Send objects to a fetching or cloning client.
    UploadPack,
    /// Accept objects and ref updates from a pushing client.
    ReceivePack,
}

/// Run a server program against the repository at `directory`.
///
/// # Parameters
///
/// - `service`: upload-pack or receive-pack.
/// - `directory`: the repository to serve (a bare repository, a working tree,
///   or the same path with `.git` appended).
/// - `stateless_rpc`: answer one request without advertising first (smart HTTP).
/// - `advertise_refs`: print the advertisement and exit (smart HTTP discovery).
///
/// # Errors
///
/// Fails when `directory` is not a repository or the client breaks the protocol.
pub fn run(
    service: Service,
    directory: &str,
    stateless_rpc: bool,
    advertise_refs: bool,
) -> Result<()> {
    let options = RepositoryOptions::with_environment(crate::context::environment());
    let repo = Repository::open(Path::new(directory), &options.clone().for_serving())
        .with_context(|| format!("opening repository '{directory}'"))?;
    let config = ConfigSet::load(
        repo.environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(repo.environment(), Some(repo.git_dir())),
    )
    .unwrap_or_default();
    let hidden_refs = match service {
        Service::UploadPack => grit_lib::refs::hide_refs::hide_ref_patterns_uploadpack(&config),
        Service::ReceivePack => grit_lib::refs::hide_refs::hide_ref_patterns_receive(&config),
    };
    let protocol = std::env::var("GIT_PROTOCOL")
        .map(|v| ProtocolVersion::from_git_protocol(&v))
        .unwrap_or_default();
    let mut opts = ServeOptions::default()
        .with_protocol(protocol)
        .with_stateless_rpc(stateless_rpc)
        .with_advertise_refs(advertise_refs)
        .with_agent(format!("grit/{}", env!("CARGO_PKG_VERSION")))
        .with_hidden_refs(hidden_refs)
        .with_lazy_fetch(ServeOptions::lazy_fetch_from_environment(
            repo.environment(),
        ));
    // Only a server allowed to fetch missing promised objects gets a fetcher.
    if matches!(service, Service::UploadPack) {
        opts.attach_promisor_fetcher(
            &repo,
            Some(std::sync::Arc::new(
                grit_lib::objects::promisor::DefaultPromisorFetcher::new(std::sync::Arc::new(
                    crate::context::http_client_factory(),
                )),
            )),
        );
    }

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = BufWriter::new(std::io::stdout().lock());
    match service {
        Service::UploadPack => {
            let _summary = serve::UploadPack::new(&repo, &mut opts).run(&mut input, &mut output)?;
        }
        Service::ReceivePack => {
            let _interrupt = crate::interrupt::InterruptCancel::install();
            let policy = ReceivePolicy::from_config(&config);
            let guard = grit_protocol::PushGuard::for_repository(&repo);
            match serve::ReceivePack::new(&repo, &opts, &policy)
                .with_hook(&guard)
                .with_cancel(&crate::interrupt::process_cancel_token())
                .run(&mut input, &mut output)
            {
                Ok(_summary) => {}
                Err(LibError::Cancelled) => {
                    return Err(anyhow::Error::new(LibError::Cancelled));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
    stdio::io_result(output.flush()).context("writing the response")?;
    Ok(())
}

/// Dispatch parsed CLI args for `Serve`.
pub fn dispatch(
    args: crate::args::ServeCommandArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let ServeCommandArgs {
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
    } = args;
    {
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
}

/// Dispatch parsed CLI args for `UploadPack`.
pub fn dispatch_upload_pack(
    args: crate::args::UploadPackArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::CommandEnv;
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts: _,
        ctx: _,
        process_env: _,
    } = env;
    let args = args.inner;
    {
        commands::serve::run(
            commands::serve::Service::UploadPack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        )?;
        Ok(ExitStatus::Success)
    }
}

/// Dispatch parsed CLI args for `ReceivePack`.
pub fn dispatch_receive_pack(
    args: crate::args::ReceivePackArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::CommandEnv;
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts: _,
        ctx: _,
        process_env: _,
    } = env;
    let args = args.inner;
    {
        commands::serve::run(
            commands::serve::Service::ReceivePack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        )?;
        Ok(ExitStatus::Success)
    }
}

//! `grit init` — create a new, empty repository.

use std::path::PathBuf;

use anyhow::{Context, Result};
use grit_lib::config::git_path::user_display_path;
use grit_lib::objects::HashAlgo;
use grit_lib::prelude::RepositoryOptions;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};

pub use crate::render::init::InitOutcome;

pub fn run(
    path: Option<String>,
    bare: bool,
    ref_format: RefStorageFormat,
    initial_branch: Option<String>,
    separate_git_dir: Option<PathBuf>,
    object_format: Option<HashAlgo>,
) -> Result<InitOutcome> {
    let path = PathBuf::from(path.unwrap_or_else(|| ".".to_owned()));
    let mut init_options = InitOptions::new()
        .with_bare(bare)
        .with_ref_storage(ref_format)
        .with_repository_options(RepositoryOptions::from_process());
    if let Some(branch) = initial_branch {
        init_options = init_options.with_initial_branch(branch);
    }
    if let Some(format) = object_format {
        init_options = init_options.with_object_format(format);
    }
    let separate_git_dir_for_json = separate_git_dir.clone();
    if let Some(separate) = separate_git_dir {
        init_options = init_options.with_separate_git_dir(Some(separate));
    }

    let (repo, outcome) = Repository::init(&path, &init_options)
        .with_context(|| format!("could not initialize a repository at {}", path.display()))?;

    Ok(InitOutcome {
        initialized: !outcome.reinitialized,
        reinitialized: outcome.reinitialized,
        path: user_display_path(repo.git_dir()),
        bare,
        branch: outcome.initial_branch,
        ref_format: ref_format.to_string(),
        object_format: repo.odb().hash_algo().name().to_owned(),
        git_dir: separate_git_dir_for_json.map(|p| user_display_path(&p)),
    })
}

/// Dispatch parsed CLI args for `Init`.
pub fn dispatch(
    args: crate::args::InitArgs,
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
    let InitArgs {
        path,
        bare,
        ref_format,
        initial_branch,
        separate_git_dir,
        object_format,
    } = args;
    emit_success(
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
    )
}

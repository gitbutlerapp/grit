//! Route a parsed [`crate::args::Command`] to command implementations.

pub(crate) mod interaction;
pub(crate) mod support;

pub(crate) use interaction::CommandInteraction;

pub(crate) use support::{
    emit_rebase_outcome, emit_success, emit_success_quiet, finish_hunk_error, is_broken_pipe,
    reject_non_human_plumbing, talks_to_remotes, validate_cli, writes_objects, CommandEnv,
};

use anyhow::Result;

use crate::args::{Command, StatusArgs};
use crate::commands;
use crate::exit_status::ExitStatus;

pub fn dispatch(cli: crate::args::Cli, env: CommandEnv<'_>) -> Result<ExitStatus> {
    match cli.command.unwrap_or(Command::Status {
        args: StatusArgs::default(),
    }) {
        Command::Init { args } => commands::init::dispatch(args, env),
        Command::Checkout { args } => commands::checkout::dispatch(args, env),
        Command::Checkin { args } => commands::checkin::dispatch(args, env),
        Command::Integrate { args } => commands::checkin::dispatch_integrate(args, env),
        Command::Sync { args } => commands::sync::dispatch(args, env),
        Command::Share { args } => commands::share::dispatch(args, env),
        Command::Slice { args } => commands::slice::dispatch(args, env),
        Command::Clone { args } => commands::clone::dispatch(args, env),
        Command::Remote { args } => commands::remote::dispatch(args, env),
        Command::Log { args } => commands::log::dispatch(args, env),
        Command::Revs { args } => commands::revs::dispatch(args, env),
        Command::MergeBase { args } => commands::merge_base_cmd::dispatch(args, env),
        Command::Reflog { args } => commands::reflog::dispatch(args, env),
        Command::Blame { args } => commands::blame::dispatch(args, env),
        Command::Describe { args } => commands::describe::dispatch(args, env),
        Command::Diff { args } => commands::diff::dispatch(args, env),
        Command::Show { args } => commands::show::dispatch(args, env),
        Command::Status { args } => commands::status::dispatch(args, env),
        Command::Shortlog { args } => commands::shortlog::dispatch(args, env),
        Command::Add { args } => commands::add::dispatch(args, env),
        Command::Rm { args } => commands::rm::dispatch(args, env),
        Command::Mv { args } => commands::mv::dispatch(args, env),
        Command::Clean { args } => commands::clean::dispatch(args, env),
        Command::Repack { args } => commands::repack::dispatch(args, env),
        Command::Gc { args } => commands::gc::dispatch(args, env),
        Command::Fsck { args } => commands::fsck::dispatch(args, env),
        Command::Ignore { args } => commands::ignore::dispatch(args, env),
        Command::Hooks { args } => commands::hooks::dispatch(args, env),
        Command::PatchId { args } => commands::patch_id::dispatch(args, env),
        Command::Resolve { args } => commands::resolve::dispatch(args, env),
        Command::Cat { args } => commands::cat::dispatch(args, env),
        Command::Commit { args } => commands::commit::dispatch(args, env),
        Command::Branch { args } => commands::branch::dispatch(args, env),
        Command::Switch { args } => commands::switch::dispatch(args, env),
        Command::Reset { args } => commands::reset::dispatch(args, env),
        Command::Merge { args } => commands::merge::dispatch(args, env),
        Command::Pick { args } => commands::pick::dispatch(args, env),
        Command::Revert { args } => commands::revert::dispatch(args, env),
        Command::History { args } => commands::history::dispatch(args, env),
        Command::Stash { args } => commands::stash::dispatch(args, env),
        Command::Restore { args } => commands::restore::dispatch(args, env),
        Command::Fetch { args } => commands::fetch::dispatch(args, env),
        Command::Pull { args } => commands::pull::dispatch(args, env),
        Command::Rebase { args } => commands::history::dispatch_rebase(args, env),
        Command::Push { args } => commands::push::dispatch(args, env),
        Command::Bundle { args } => commands::bundle::dispatch(args, env),
        Command::Notes { args } => commands::notes::dispatch(args, env),
        Command::Tag { args } => commands::tag::dispatch(args, env),
        Command::Worktree { args } => commands::worktree::dispatch(args, env),
        Command::Auth { args } => commands::auth::dispatch(args, env),
        Command::Update { args } => commands::update::dispatch(args, env),
        Command::Skill { args } => commands::skill::dispatch(args, env),
        Command::Completions { args } => commands::completions::dispatch(args, env),
        Command::Config { args } => commands::config::dispatch(args, env),
        Command::Manager { args } => commands::manager::dispatch(args, env),
        Command::Serve { args } => commands::serve::dispatch(args, env),
        Command::UploadPack { args } => commands::serve::dispatch_upload_pack(args, env),
        Command::ReceivePack { args } => commands::serve::dispatch_receive_pack(args, env),
    }
}

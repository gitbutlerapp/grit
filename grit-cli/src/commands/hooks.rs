//! `grit hooks` — list hook scripts and run them.

use anyhow::Result;
use grit_lib::repo_handles::{HookInfo, HookSource};

use crate::context;
use crate::ui::filesystem_path_for_user;

pub use crate::render::hooks::{HookEntryJson, HooksOutcome};

impl From<HookInfo> for HookEntryJson {
    fn from(info: HookInfo) -> Self {
        Self {
            name: info.name,
            path: filesystem_path_for_user(&info.path),
            executable: info.executable,
            source: match info.source {
                HookSource::HooksDir => "hooks_dir".to_owned(),
                HookSource::CoreHooksPath => "core_hooks_path".to_owned(),
            },
        }
    }
}

/// List hook scripts in the repository hooks directory.
pub fn run_list(limit: Option<usize>) -> Result<HooksOutcome> {
    let repo = context::discover()?;
    let listed = repo
        .hooks()
        .list(limit)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(HooksOutcome::List {
        hooks: listed.items.into_iter().map(HookEntryJson::from).collect(),
        truncated: listed.truncated,
    })
}

/// Run a hook by name; returns the subprocess exit code when the hook ran.
pub fn run_hook_by_name(name: &str, args: &[String]) -> Result<HooksOutcome> {
    let repo = context::discover()?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = repo.hooks().run(name, &arg_refs, None)?;
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let output = result.output();
    Ok(HooksOutcome::Run {
        name: name.to_owned(),
        exit_code: result.status().unwrap_or(0),
        executed: result.was_executed(),
        stdout: output.map(|o| lossy(&o.stdout)).unwrap_or_default(),
        stderr: output.map(|o| lossy(&o.stderr)).unwrap_or_default(),
    })
}

/// Process exit code for `grit hooks run`.
#[must_use]
pub fn run_exit_code(outcome: &HooksOutcome) -> i32 {
    match outcome {
        HooksOutcome::Run { exit_code, .. } => *exit_code,
        HooksOutcome::List { .. } => 0,
    }
}

/// Dispatch parsed CLI args for `Hooks`.
pub fn dispatch(
    args: crate::args::HooksArgs,
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
    let HooksArgs { limit, action } = args;
    match action {
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
    }
}

//! `grit hooks` — list hook scripts and run them.

use anyhow::Result;
use grit_lib::command_runner::hooks::{run_hook, HookResult};
use grit_lib::repo_handles::{HookInfo, HookSource};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit hooks list` or `grit hooks run`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HooksOutcome {
    /// Installed hook scripts in the resolved hooks directory.
    List { hooks: Vec<HookEntryJson> },
    /// A hook subprocess finished (see `exit_code`).
    Run {
        name: String,
        exit_code: i32,
        executed: bool,
    },
}

/// One hook script on disk.
#[derive(Serialize, JsonSchema, Clone)]
pub struct HookEntryJson {
    pub name: String,
    pub path: String,
    pub executable: bool,
    pub source: String,
}

impl From<HookInfo> for HookEntryJson {
    fn from(info: HookInfo) -> Self {
        Self {
            name: info.name,
            path: info.path.display().to_string(),
            executable: info.executable,
            source: match info.source {
                HookSource::HooksDir => "hooks_dir".to_owned(),
                HookSource::CoreHooksPath => "core_hooks_path".to_owned(),
            },
        }
    }
}

impl HumanRender for HooksOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            HooksOutcome::List { hooks } => {
                for hook in hooks {
                    let exec = if hook.executable { "yes" } else { "no" };
                    outln!(
                        w,
                        "{}  {}  executable={}  {}",
                        hook.name,
                        hook.path,
                        exec,
                        hook.source
                    );
                }
            }
            HooksOutcome::Run { .. } => {}
        }
        Ok(())
    }
}

impl MarkdownRender for HooksOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            HooksOutcome::List { hooks } => {
                outln!(w, "## Hooks\n");
                if hooks.is_empty() {
                    outln!(w, "No hook scripts found.");
                    return Ok(());
                }
                outln!(w, "| Name | Path | Executable | Source |");
                outln!(w, "| --- | --- | --- | --- |");
                for hook in hooks {
                    outln!(
                        w,
                        "| `{}` | `{}` | {} | {} |",
                        hook.name,
                        hook.path,
                        hook.executable,
                        hook.source
                    );
                }
            }
            HooksOutcome::Run {
                name,
                exit_code,
                executed,
            } => {
                outln!(w, "## Hook run\n");
                outln!(w, "- **name:** `{name}`");
                outln!(w, "- **executed:** {executed}");
                outln!(w, "- **exit_code:** {exit_code}");
            }
        }
        Ok(())
    }
}

/// List hook scripts in the repository hooks directory.
pub fn run_list() -> Result<HooksOutcome> {
    let repo = context::discover()?;
    let hooks = repo.hooks().list().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(HooksOutcome::List {
        hooks: hooks.into_iter().map(HookEntryJson::from).collect(),
    })
}

/// Run a hook by name; returns the subprocess exit code when the hook ran.
pub fn run_hook_by_name(name: &str, args: &[String]) -> Result<HooksOutcome> {
    let repo = context::discover()?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = run_hook(&repo, name, &arg_refs, None);
    let (exit_code, executed) = match result {
        HookResult::Success => (0, true),
        HookResult::NotFound => (0, false),
        HookResult::Failed(code) => (code, true),
    };
    Ok(HooksOutcome::Run {
        name: name.to_owned(),
        exit_code,
        executed,
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

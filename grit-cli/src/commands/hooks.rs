//! `grit hooks` — list hook scripts and run them.

use anyhow::Result;
use grit_lib::command_runner::hooks::run_hook;
use grit_lib::repo_handles::{HookInfo, HookSource};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit hooks list` or `grit hooks run`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HooksOutcome {
    /// Installed hook scripts in the resolved hooks directory.
    List {
        hooks: Vec<HookEntryJson>,
        /// True when more hook scripts existed than returned (`-n` / `--limit`).
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        truncated: bool,
    },
    /// A hook subprocess finished (see `exit_code`).
    Run {
        name: String,
        exit_code: i32,
        executed: bool,
        /// What the hook wrote to stdout (lossy UTF-8). Empty in human mode, where the hook
        /// writes to the terminal directly.
        stdout: String,
        /// What the hook wrote to stderr (lossy UTF-8). Empty in human mode.
        stderr: String,
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
            HooksOutcome::List { hooks, .. } => {
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
            HooksOutcome::List { hooks, .. } => {
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
                stdout,
                stderr,
            } => {
                outln!(w, "## Hook run\n");
                outln!(w, "- **name:** `{name}`");
                outln!(w, "- **executed:** {executed}");
                outln!(w, "- **exit_code:** {exit_code}");
                crate::markdown::render_hook_output(w, stdout, stderr)?;
            }
        }
        Ok(())
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
    let result = run_hook(&repo, name, &arg_refs, None)?;
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

//! Render `grit hooks` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

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
                crate::render::list::render_hook_output(w, stdout, stderr)?;
            }
        }
        Ok(())
    }
}

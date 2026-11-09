//! Render `grit ignore check` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit ignore check`.
#[derive(Serialize, JsonSchema)]
pub struct IgnoreCheckOutcome {
    /// Whether at least one queried path is ignored.
    pub any_ignored: bool,
    /// Per-path results (filtered by `--all`; default lists only ignored paths).
    pub entries: Vec<IgnoreCheckEntryJson>,
}

#[derive(Serialize, JsonSchema)]
pub struct IgnoreCheckEntryJson {
    pub path: String,
    pub ignored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<IgnoreMatchJson>,
}

#[derive(Serialize, JsonSchema)]
pub struct IgnoreMatchJson {
    pub source: IgnoreSourceJson,
    pub line_number: usize,
    pub pattern_text: String,
    pub negative: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IgnoreSourceJson {
    PerDirectory { path: String },
    InfoExclude,
    CoreExcludesFile { path: String },
}

pub(crate) fn source_display(source: &IgnoreSourceJson) -> String {
    match source {
        IgnoreSourceJson::PerDirectory { path } => path.clone(),
        IgnoreSourceJson::InfoExclude => ".git/info/exclude".to_owned(),
        IgnoreSourceJson::CoreExcludesFile { path } => path.clone(),
    }
}

impl HumanRender for IgnoreCheckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            if entry.ignored {
                if let Some(m) = &entry.matched {
                    let source = source_display(&m.source);
                    outln!(
                        w,
                        "{}  ← {}:{} {}",
                        entry.path,
                        source,
                        m.line_number,
                        m.pattern_text
                    );
                } else {
                    outln!(w, "{}", entry.path);
                }
            } else {
                outln!(w, "{}", entry.path);
            }
        }
        Ok(())
    }
}

impl MarkdownRender for IgnoreCheckOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Ignore check\n");
        outln!(w, "- **any_ignored:** {}\n", self.any_ignored);
        if self.entries.is_empty() {
            outln!(w, "No matching paths to show.");
            return Ok(());
        }
        for entry in &self.entries {
            if entry.ignored {
                if let Some(m) = &entry.matched {
                    let source = source_display(&m.source);
                    outln!(
                        w,
                        "- `{}` ← `{}:{}` `{}`",
                        entry.path,
                        source,
                        m.line_number,
                        m.pattern_text
                    );
                } else {
                    outln!(w, "- `{}` (ignored)", entry.path);
                }
            } else {
                outln!(w, "- `{}` (not ignored)", entry.path);
            }
        }
        Ok(())
    }
}

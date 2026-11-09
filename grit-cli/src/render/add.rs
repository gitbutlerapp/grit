//! `grit add` outcomes and rendering.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use grit_lib::porcelain::add::StageOutcome;
use grit_lib::porcelain::hunk_select::StageHunksOutcome;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

const HUMAN_PATH_LIMIT: usize = 10;

/// Result of `grit add`: paths staged or that would be staged.
#[derive(Serialize, JsonSchema)]
pub struct AddOutcome {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    pub intent_to_add: Vec<String>,
    pub dry_run: bool,
    /// Total paths touched (same as `staged`; kept for one release).
    pub count: usize,
    /// How many paths were staged (alias of `count` for scripts written before path lists).
    pub staged: usize,
    /// Hunk ids staged per path when `--hunks` was used (empty object when unused).
    pub staged_hunks: BTreeMap<String, Vec<String>>,
    /// Line ranges staged per path when `--lines` was used (`start-end` strings; empty when unused).
    pub staged_lines: BTreeMap<String, Vec<String>>,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) no_paths: bool,
    /// Nested repositories newly recorded as gitlinks (mode 160000).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded_gitlinks: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) path_display: Option<PathDisplayOptions>,
}

impl AddOutcome {
    pub(crate) fn from_stage(outcome: StageOutcome, dry_run: bool, no_paths: bool) -> Self {
        let count = outcome.total();
        Self {
            added: outcome.added,
            modified: outcome.modified,
            removed: outcome.removed,
            intent_to_add: outcome.intent_to_add,
            dry_run,
            count,
            staged: count,
            staged_hunks: BTreeMap::new(),
            staged_lines: BTreeMap::new(),
            no_paths,
            embedded_gitlinks: outcome.embedded_gitlinks,
            path_display: None,
        }
    }

    pub(crate) fn from_hunks(outcome: StageHunksOutcome) -> Self {
        let staged_hunks: BTreeMap<String, Vec<String>> = outcome
            .staged_hunks
            .into_iter()
            .map(|(path, ids)| (path, ids.into_iter().map(|id| id.as_str()).collect()))
            .collect();
        let count = outcome.added.len() + outcome.modified.len() + outcome.removed.len();
        Self {
            added: outcome.added,
            modified: outcome.modified,
            removed: outcome.removed,
            intent_to_add: Vec::new(),
            dry_run: outcome.dry_run,
            count,
            staged: count,
            staged_hunks,
            staged_lines: outcome.staged_lines,
            no_paths: false,
            embedded_gitlinks: Vec::new(),
            path_display: None,
        }
    }

    pub(crate) fn with_path_display(mut self, path_display: Option<PathDisplayOptions>) -> Self {
        self.path_display = path_display;
        self
    }

    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
}

impl HumanRender for AddOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        crate::embedded_gitlink::emit_embedded_gitlink_warnings(
            ctx.mode,
            &self.embedded_gitlinks,
            self.dry_run,
            self.path_display.as_ref(),
        );
        let prefix = if self.dry_run {
            "Would stage"
        } else {
            "Staged"
        };
        let paths = self.all_paths();
        match paths.len() {
            0 if self.no_paths => outln!(w, "Nothing to stage — working tree clean."),
            0 => {}
            1 => outln!(w, "{prefix} {}", self.format_path(paths[0])),
            n => {
                for path in paths.iter().take(HUMAN_PATH_LIMIT) {
                    outln!(w, "{prefix} {}", self.format_path(path));
                }
                if n > HUMAN_PATH_LIMIT {
                    outln!(w, "… and {} more", n - HUMAN_PATH_LIMIT);
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for AddOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        crate::embedded_gitlink::emit_embedded_gitlink_warnings(
            ctx.mode,
            &self.embedded_gitlinks,
            self.dry_run,
            self.path_display.as_ref(),
        );
        markdown::print_add_outcome(w, self, self.path_display.as_ref())?;
        Ok(())
    }
}

impl AddOutcome {
    fn all_paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        out.extend(self.added.iter().map(String::as_str));
        out.extend(self.modified.iter().map(String::as_str));
        out.extend(self.removed.iter().map(String::as_str));
        out.extend(self.intent_to_add.iter().map(String::as_str));
        out
    }
}

//! `grit diff` — show changes as a delta-style, dual-line-numbered diff.

mod convert;
pub mod dispatch;
mod run;
mod stream;

pub use dispatch::dispatch;

pub(crate) use run::resolve_commit_diff_pathspecs;
pub use run::{
    cli_outcome_from_lib, diff_for_commit, emit_commits_log_patch_text, emit_show_log_patch_text,
    outcome_from_tree_entries, render_log_commit_diff, run, streamed_log_patch_text_outcome,
    CommitDiffOptions, ShowLogPatchTextParams,
};

use std::io::{IsTerminal, Write};

use grit_lib::diff::DiffTotals;
use schemars::JsonSchema;
use serde::Serialize;

use crate::diff_format::DiffFormatArgs;
use crate::output::{HumanRender, MarkdownRender, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;
use crate::ui::PathDisplayOptions;

#[allow(unused_imports)]
pub use crate::render::diff::{
    diffstat_from_files, file_diff_status_label, render_file, render_markdown_stat,
    render_name_only, render_name_status, render_numstat, render_shortstat, render_stat,
    render_stat_with_leading_blank, summary_line, write_file_diff_metadata_human, DiffStat,
    FileDiff, Hunk, Line, LineKind, LineStatJson, Segment,
};

/// Aggregate line counts for `--stat`, `--numstat`, `--shortstat`, and `-p` JSON.
#[derive(Serialize, JsonSchema, Clone, Copy)]
pub struct DiffTotalsJson {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

impl From<DiffTotals> for DiffTotalsJson {
    fn from(totals: DiffTotals) -> Self {
        Self {
            files_changed: totals.files_changed,
            insertions: totals.insertions,
            deletions: totals.deletions,
        }
    }
}

/// Result of `grit diff`.
#[derive(Serialize, JsonSchema)]
pub struct DiffOutcome {
    /// Whether any selected change exists (matches `grit diff --quiet` / `--exit-code`).
    pub changed: bool,
    pub files: Vec<FileDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) format: DiffFormatArgs,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) path_display: Option<PathDisplayOptions>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) body_streamed: bool,
    /// When true, finish must not recompute [`Self::changed`] from [`Self::files`].
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) preserve_changed: bool,
}

/// How patch bodies are delivered for ODB-backed tree diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchBodies {
    /// Only record whether anything changed (`--quiet`, `--exit-code`, `--patch-text` metadata).
    MetadataOnly,
    /// Stream each file patch body to stdout as it is computed (human or Markdown).
    Stream,
    /// Hold file rows in memory (JSON aggregation or tests).
    Collect,
}

/// Inputs for `grit diff` (flags and revision arguments).
pub struct DiffRunOptions {
    pub no_index: bool,
    pub staged: bool,
    pub unstaged: bool,
    pub against: Option<String>,
    pub commits: Vec<String>,
    pub pathspecs: TrailingPathspecArgs,
    pub format: DiffFormatArgs,
    pub line_format: crate::diff_args::DiffCompareArgs,
    pub quiet: bool,
}

/// Per-invocation rendering and hunk-id options (set in dispatch, not in [`DiffRunOptions`]).
#[derive(Clone, Copy, Debug)]
pub struct DiffRunContext {
    pub word_level_inline: bool,
    pub emit_json_hunk_ids: bool,
    /// When false with [`PatchBodies::Collect`], file rows stay in memory for `--json --filter`.
    pub json_stream: bool,
    pub patch_bodies: PatchBodies,
    pub render: RenderContext,
}

/// Exit status when `grit diff` fails while `--exit-code` or `--quiet` is set.
pub const DIFF_ERROR_EXIT: i32 = 2;

/// Whether human diff output should compute word-level inline emphasis.
#[must_use]
pub fn diff_word_level_inline_for_output() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

/// Process exit code for `--exit-code` / `--quiet`: 0 when clean, 1 when changed.
#[must_use]
pub fn exit_code(outcome: &DiffOutcome) -> i32 {
    if outcome.changed {
        1
    } else {
        0
    }
}

impl DiffOutcome {
    #[must_use]
    pub(crate) fn format_flags(&self) -> DiffFormatArgs {
        self.format
    }

    pub(crate) fn path_display_opts(&self) -> Option<&PathDisplayOptions> {
        self.path_display.as_ref()
    }

    pub(crate) fn render_human_diff(
        &self,
        w: &mut dyn Write,
        empty_message: Option<&str>,
        color: bool,
    ) -> anyhow::Result<()> {
        run::render_human_diff(self, w, empty_message, color)
    }

    pub(crate) fn render_markdown_diff(&self, w: &mut dyn Write) -> anyhow::Result<()> {
        run::render_markdown_diff(self, w)
    }
}

impl HumanRender for DiffOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> anyhow::Result<()> {
        self.render_human_diff(w, Some("No changes."), ctx.color)
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> anyhow::Result<()> {
        self.render_markdown_diff(w)
    }
}

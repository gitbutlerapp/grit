//! `grit diff` — show changes as a delta-style, dual-line-numbered diff.
//!
//! With no argument it shows all uncommitted changes (the worktree and index
//! against HEAD's tree). Pass `--unstaged` for index against worktree only, or
//! `--staged` for the index against HEAD. With one commit-ish
//! it shows the change that commit introduced (its first parent's tree against
//! its own). Two commits, or a two-dot range such as `A..B`, diff those
//! commits' trees. Use `--against <ref>` to compare the worktree to another
//! ref's tree instead of HEAD.
//!
//! The human rendering imitates [delta](https://github.com/dandavison/delta): a
//! bold file header, a hunk header showing the enclosing definition, two
//! line-number columns (old / new), red/green line backgrounds, and brighter
//! intra-line word highlights — no `+`/`-` patch markers. Color is emitted only
//! on a TTY (honoring `NO_COLOR`); piped output is plain text with `+`/`-`.

use anyhow::{Context, Result};
use grit_lib::diff::{
    apply_diff_filter, build_file_diffs, commit_diff_files, diff_commits_with_renames_and_policy,
    diff_trees_with_renames_and_policy, for_each_tree_diff_structured,
    hunk_ids_with_duplicate_peers, load_object_changes, peer_hunks_by_primary_path,
    peer_scope_includes_path, resolve_diff_request, sort_entries_by_path,
    structured::{format_hunk_range, StructuredLineKind},
    tree_diff_structured_parallel, write_commit_log_patch, write_commits_log_patch, write_patch,
    write_patch_resolved, DiffArguments, DiffDetail, DiffEntry, DiffFilesOutput, DiffMerges,
    DiffOptions, DiffRequest, DiffSide, DiffTarget, DiffTotals, FileChange, LineStat,
    ListedFileDiff, LogPatchCommitHeader, LogPatchOptions, PatchResolvedSide, PatchSide,
    PatchTextOptions, ResolveDiffArgumentsError, TreeDiffStructured, UncommittedDiff,
};
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ParallelOp;
use grit_lib::state::resolve_head;
use grit_lib::worktree::pathspec::pathdiff;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::IsTerminal;
use std::io::Write;

use crate::context;
use crate::diff_args::DiffCompareArgs;
use crate::diff_format::DiffFormatArgs;
use crate::diff_render::{
    self, diffstat_from_files, render_name_only, render_name_status, render_numstat,
    render_shortstat, render_stat,
};
use crate::markdown;
use crate::output::{outln, ChangeStatus, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;
use crate::ui::{self, PathDisplayOptions};

/// Lines of unchanged context to show around each change.
const CONTEXT_LINES: usize = 3;
/// Tab stop width used when expanding tabs for display.
const TAB_WIDTH: usize = 8;

// --- ANSI styling (256-color, delta-ish) -----------------------------------
const RESET: &str = "\x1b[0m";
const BG_DEL: &str = "48;5;52"; // dark red
const BG_DEL_EMPH: &str = "48;5;88"; // brighter red
const BG_ADD: &str = "48;5;22"; // dark green
const BG_ADD_EMPH: &str = "48;5;28"; // brighter green
const FG_DIM: &str = "38;5;244"; // gutter gray
const FG_DEL_NUM: &str = "38;5;167"; // removed line number
const FG_ADD_NUM: &str = "38;5;71"; // added line number
                                    // Explicit near-white foreground for the changed (colored-background) lines.
                                    // Our backgrounds are always dark, so a light fg stays readable on both light-
                                    // and dark-themed terminals (the default fg would be dark-on-dark in light mode).
const FG_ON_DIFF: &str = "38;5;231";

/// Aggregate line counts for `--stat` / `--numstat` JSON.
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

/// Per-file line counts in JSON when stat modes are active.
#[derive(Serialize, JsonSchema, Clone, Copy)]
pub struct LineStatJson {
    pub insertions: usize,
    pub deletions: usize,
}

impl From<LineStat> for LineStatJson {
    fn from(stat: LineStat) -> Self {
        Self {
            insertions: stat.insertions,
            deletions: stat.deletions,
        }
    }
}

impl From<LineStatJson> for LineStat {
    fn from(stat: LineStatJson) -> Self {
        Self {
            insertions: stat.insertions,
            deletions: stat.deletions,
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
    format: DiffFormatArgs,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
    #[serde(skip)]
    #[schemars(skip)]
    body_streamed: bool,
    /// When true, [`finish_outcome`] must not recompute [`Self::changed`] from [`Self::files`].
    #[serde(skip)]
    #[schemars(skip)]
    preserve_changed: bool,
}

/// One changed file.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FileDiff {
    /// Display path (the new path, or the old path for a deletion).
    pub path: String,
    /// Pre-rename path, when different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    /// Old git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// True when old side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub old_encoding_lossy: bool,
    /// True when new side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub new_encoding_lossy: bool,
    pub binary: bool,
    /// True when the blob OID changed (not a mode-only touch on the same content).
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub content_changed: bool,
    /// Rename/copy similarity (0–100).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<u32>,
    /// Line counts when `--stat` / `--numstat` / `--shortstat` is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub stat: Option<LineStatJson>,
    /// Per-parent combined status letters (`MM`, `DD`, …) for merge `--cc` name-status.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub combined_status: Option<String>,
    /// Patch hunks; omitted in JSON unless the output format is a full patch.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<Hunk>>,
}

impl FileDiff {
    /// Line counts from the stat, or counted from the patch hunks when only those were built.
    #[must_use]
    pub fn line_stat(&self) -> LineStat {
        match self.stat {
            Some(stat) => stat.into(),
            None => self
                .hunks
                .iter()
                .flatten()
                .flat_map(|hunk| &hunk.lines)
                .map(|line| StructuredLineKind::from(line.kind))
                .collect(),
        }
    }
}

/// A contiguous run of changes plus surrounding context.
#[derive(Clone, Serialize, JsonSchema)]
pub struct Hunk {
    /// 1-based first old/new line numbers in the hunk.
    pub old_start: usize,
    pub new_start: usize,
    /// Lines on the old side in this hunk (context + removals).
    pub old_lines: usize,
    /// Lines on the new side in this hunk (context + additions).
    pub new_lines: usize,
    /// The enclosing definition line (function/struct/…), when found.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Combined (`--cc`) hunk header; when set, [`Self::old_start`] / range fields are omitted from human output.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub combined_header: Option<String>,
    /// Stable id for `--hunks` selection (unstaged and staged diffs).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 0-based hunk index within the file (unstaged and staged diffs).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub lines: Vec<Line>,
}

/// JSON schema shape for [`Line`] (`text` or `segments`, never both).
#[derive(JsonSchema)]
#[allow(dead_code)]
struct LineJsonSchema {
    kind: LineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    new: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    segments: Option<Vec<Segment>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    no_newline_at_eof: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    parents: Option<Vec<String>>,
}

impl JsonSchema for Line {
    fn schema_name() -> String {
        "Line".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        LineJsonSchema::json_schema(gen)
    }
}

/// One rendered line of a hunk.
#[derive(Clone)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    pub no_newline_at_eof: bool,
    /// Combined merge parent columns (`+`, `-`, or space), one entry per parent.
    pub parents: Option<Vec<String>>,
}

impl Serialize for Line {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let field_count = 3
            + usize::from(self.old.is_some())
            + usize::from(self.new.is_some())
            + usize::from(self.no_newline_at_eof)
            + usize::from(self.parents.is_some())
            + 1;
        let mut state = serializer.serialize_struct("Line", field_count)?;
        state.serialize_field("kind", &self.kind)?;
        if let Some(old) = self.old {
            state.serialize_field("old", &old)?;
        }
        if let Some(new) = self.new {
            state.serialize_field("new", &new)?;
        }
        if segments_have_emphasis(&self.segments) {
            state.serialize_field("segments", &self.segments)?;
        } else {
            state.serialize_field("text", &line_text(&self.segments))?;
        }
        if self.no_newline_at_eof {
            state.serialize_field("no_newline_at_eof", &true)?;
        }
        if let Some(parents) = &self.parents {
            state.serialize_field("parents", parents)?;
        }
        state.end()
    }
}

fn line_text(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

fn segments_have_emphasis(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.emphasis)
}

#[derive(Serialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

impl From<LineKind> for StructuredLineKind {
    fn from(kind: LineKind) -> Self {
        match kind {
            LineKind::Context => Self::Context,
            LineKind::Add => Self::Add,
            LineKind::Del => Self::Del,
        }
    }
}

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Clone, Serialize, JsonSchema)]
pub struct Segment {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub emphasis: bool,
}

/// How commit-range patch bodies are delivered (memory vs JSON aggregation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffBodyDelivery {
    /// Build every file diff in memory (for `--json` and tests).
    Collect,
    /// Stream each file to stdout as it is computed (human or markdown).
    Stream(OutputMode),
    /// Only record whether anything changed (`--quiet`, `--exit-code`).
    MetadataOnly,
}

/// Inputs for `grit diff` (flags and revision arguments).
pub struct DiffRunOptions {
    /// Compare two filesystem paths outside any repository (Git `--no-index`).
    pub no_index: bool,
    pub staged: bool,
    pub unstaged: bool,
    pub against: Option<String>,
    pub commits: Vec<String>,
    /// Trailing pathspec argv and optional `--pathspec-from-file`.
    pub pathspecs: TrailingPathspecArgs,
    /// Output format (`--stat`, `--name-only`, …).
    pub format: DiffFormatArgs,
    /// Line-diff flags (`-M`, `--no-renames`, `--diff-filter`, …).
    pub line_format: DiffCompareArgs,
    /// Intra-line word highlights in structured hunks (human TTY output only).
    pub word_level_inline: bool,
    /// Print nothing; `main` then exits with the `--exit-code` status.
    pub quiet: bool,
    /// Assign stable hunk ids (loads opposite-side peers when needed). True for `--json` patch output only.
    pub emit_json_hunk_ids: bool,
    /// Patch body delivery for ODB-backed tree diffs (commit ranges).
    pub body_delivery: DiffBodyDelivery,
}

/// Exit status when `grit diff` fails while `--exit-code` or `--quiet` is set.
pub const DIFF_ERROR_EXIT: i32 = 2;

/// Process exit code for `--exit-code` / `--quiet`: 0 when clean, 1 when changed.
#[must_use]
pub fn exit_code(outcome: &DiffOutcome) -> i32 {
    if outcome.changed {
        1
    } else {
        0
    }
}

/// Whether human diff output should compute word-level inline emphasis.
#[must_use]
pub fn diff_word_level_inline_for_output() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn attach_path_display(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    mut outcome: DiffOutcome,
) -> DiffOutcome {
    outcome.path_display = PathDisplayOptions::from_repo(repo).ok();
    outcome.format = format;
    outcome
}

fn finish_no_index_outcome(format: DiffFormatArgs, mut outcome: DiffOutcome) -> DiffOutcome {
    outcome.changed = !outcome.files.is_empty();
    if format.detail() == DiffDetail::Stat && outcome.totals.is_none() {
        outcome.totals = Some(diffstat_from_files(&outcome.files).lib().totals().into());
    }
    outcome.format = format;
    outcome
}

fn finish_outcome(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    mut outcome: DiffOutcome,
) -> DiffOutcome {
    if !outcome.preserve_changed {
        outcome.changed = !outcome.files.is_empty();
    }
    if format.detail() == DiffDetail::Stat && outcome.totals.is_none() {
        outcome.totals = Some(diffstat_from_files(&outcome.files).lib().totals().into());
    }
    attach_path_display(repo, format, outcome)
}

fn format_is_byte_list_or_stat(format: DiffFormatArgs) -> bool {
    format.human_numstat()
        || format.human_name_only()
        || format.human_name_status()
        || format.human_shortstat_only()
        || format.human_stat_block()
        || format.detail() == DiffDetail::Stat
        || matches!(
            format.detail(),
            DiffDetail::NameOnly | DiffDetail::NameStatus
        )
}

fn outcome_from_change_names(changes: Vec<FileChange>, format: DiffFormatArgs) -> DiffOutcome {
    let files = changes
        .into_iter()
        .map(|fc| FileDiff {
            path: fc.path,
            old_path: fc.old_path,
            status: fc.status.into(),
            old_mode: fc.old_mode.octal_if_present(),
            new_mode: fc.new_mode.octal_if_present(),
            old_encoding_lossy: fc.old_bytes.encoding_lossy,
            new_encoding_lossy: fc.new_bytes.encoding_lossy,
            binary: fc.binary,
            content_changed: fc.content_changed,
            similarity: fc.similarity,
            stat: None,
            combined_status: None,
            hunks: None,
        })
        .collect::<Vec<FileDiff>>();
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

/// Build a [`DiffOutcome`] from library [`DiffFilesOutput`] (for `grit log` per commit).
pub fn cli_outcome_from_lib(
    repo: &grit_lib::repo::Repository,
    output: DiffFilesOutput,
    detail: DiffDetail,
    format: DiffFormatArgs,
) -> Result<DiffOutcome> {
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_lib(output, format, detail),
    ))
}

/// Human diff detail under one `grit log` commit row (no placeholder when empty).
pub fn render_log_commit_diff(w: &mut dyn Write, outcome: &DiffOutcome) -> Result<()> {
    if outcome.files.is_empty() {
        return Ok(());
    }
    let color = diff_render::use_color();
    let fmt = outcome.format;
    if fmt.human_numstat() {
        return render_numstat(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_only() {
        return render_name_only(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_status() {
        return render_name_status(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_shortstat_only() {
        let stat = diffstat_from_files(&outcome.files);
        return render_shortstat(w, &stat);
    }
    if fmt.human_stat_block() {
        let stat = diffstat_from_files(&outcome.files);
        return render_stat(w, &stat, color, outcome.path_display.as_ref());
    }
    for file in &outcome.files {
        render_file(w, file, color, outcome.path_display.as_ref())?;
    }
    Ok(())
}

fn outcome_from_lib(
    output: DiffFilesOutput,
    format: DiffFormatArgs,
    detail: DiffDetail,
) -> DiffOutcome {
    let totals = output.totals.map(DiffTotalsJson::from);
    let files = output
        .files
        .into_iter()
        .map(|f| lib_file_to_cli(f, detail))
        .collect();
    outcome_from_files(files, totals, format)
}

fn outcome_from_files(
    files: Vec<FileDiff>,
    totals: Option<DiffTotalsJson>,
    format: DiffFormatArgs,
) -> DiffOutcome {
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn lib_file_to_cli(f: ListedFileDiff, detail: DiffDetail) -> FileDiff {
    let content_changed = f
        .stat
        .map(|s| s.insertions + s.deletions > 0)
        .unwrap_or(true);
    let old_encoding_lossy = f
        .structured
        .as_ref()
        .map(|s| s.old_encoding_lossy)
        .unwrap_or(false);
    let new_encoding_lossy = f
        .structured
        .as_ref()
        .map(|s| s.new_encoding_lossy)
        .unwrap_or(false);
    let hunks = if let Some(combined) = f.combined {
        Some(
            combined
                .hunks
                .into_iter()
                .map(|h| Hunk {
                    old_start: 0,
                    new_start: 0,
                    old_lines: 0,
                    new_lines: 0,
                    context: None,
                    combined_header: Some(h.header),
                    id: None,
                    index: None,
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l| Line {
                            kind: match l.kind {
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Context => {
                                    LineKind::Context
                                }
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Add => {
                                    LineKind::Add
                                }
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Del => {
                                    LineKind::Del
                                }
                            },
                            old: None,
                            new: None,
                            segments: vec![Segment {
                                text: l.text,
                                emphasis: false,
                            }],
                            no_newline_at_eof: false,
                            parents: Some(l.parents.iter().map(|c| c.to_string()).collect()),
                        })
                        .collect(),
                })
                .collect(),
        )
    } else {
        f.structured.map(|structured| {
            hunk_assign_ids(DiffSide::Other, &f.path, structured.hunks, &[], false)
        })
    };
    FileDiff {
        path: f.path,
        old_path: f.old_path,
        status: f.status.into(),
        old_mode: f.old_mode,
        new_mode: f.new_mode,
        old_encoding_lossy,
        new_encoding_lossy,
        binary: f.binary,
        content_changed,
        similarity: f.similarity,
        stat: f.stat.map(LineStatJson::from),
        combined_status: f.combined_status,
        hunks: if detail == DiffDetail::Patch {
            Some(hunks.unwrap_or_default())
        } else {
            None
        },
    }
}

fn outcome_from_entries(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    detail: DiffDetail,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
) -> Result<DiffOutcome> {
    if matches!(detail, DiffDetail::NameOnly | DiffDetail::NameStatus) {
        let files = entries
            .into_iter()
            .map(|entry| lib_file_to_cli(ListedFileDiff::from_metadata(entry), detail))
            .collect();
        return Ok(outcome_from_files(files, None, format));
    }
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let output = build_file_diffs(
        repo.odb(),
        entries,
        detail,
        diff_options,
        config.as_ref(),
        &thread_policy,
    )?;
    Ok(outcome_from_lib(output, format, detail))
}

/// Compare two filesystem paths outside a repository ([`grit_lib::diff::diff_paths`]).
pub fn run_no_index(opts: DiffRunOptions) -> Result<DiffOutcome> {
    if opts.staged || opts.unstaged || opts.against.is_some() {
        anyhow::bail!("--no-index cannot be combined with --staged, --unstaged, or --against");
    }
    if opts.pathspecs.pathspec_from_file.is_some() {
        anyhow::bail!("--no-index cannot be combined with --pathspec-from-file");
    }
    let mut path_args: Vec<String> = opts.commits;
    path_args.extend(opts.pathspecs.paths);
    if path_args.len() != 2 {
        anyhow::bail!(
            "--no-index requires exactly two paths (use `--` before paths that start with `-`)"
        );
    }
    let mut diff_options = DiffOptions::default();
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = opts.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    let files = grit_lib::diff::diff_paths(
        std::path::Path::new(&path_args[0]),
        std::path::Path::new(&path_args[1]),
        &diff_options,
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let cli_files: Vec<FileDiff> = files
        .into_iter()
        .map(|f| file_diff_from_lib(f, detail))
        .collect();
    Ok(finish_no_index_outcome(
        format,
        DiffOutcome {
            changed: !cli_files.is_empty(),
            files: cli_files,
            totals: None,
            format,
            path_display: None,
            body_streamed: false,
            preserve_changed: false,
        },
    ))
}

fn run_no_index_patch_text(opts: DiffRunOptions) -> Result<DiffOutcome> {
    if opts.staged || opts.unstaged || opts.against.is_some() {
        anyhow::bail!("--no-index cannot be combined with --staged, --unstaged, or --against");
    }
    let quiet = opts.quiet;
    if opts.pathspecs.pathspec_from_file.is_some() {
        anyhow::bail!("--no-index cannot be combined with --pathspec-from-file");
    }
    let mut path_args: Vec<String> = opts.commits;
    path_args.extend(opts.pathspecs.paths);
    if path_args.len() != 2 {
        anyhow::bail!(
            "--no-index requires exactly two paths (use `--` before paths that start with `-`)"
        );
    }
    let mut diff_options = DiffOptions::default();
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = opts.word_level_inline;
    let format = opts.format;
    let a = std::path::Path::new(&path_args[0]);
    let b = std::path::Path::new(&path_args[1]);
    let changed = if quiet {
        grit_lib::diff::write_paths_patch(a, b, &diff_options, &mut std::io::sink())?
    } else {
        grit_lib::diff::write_paths_patch(a, b, &diff_options, &mut std::io::stdout())?
    };
    Ok(DiffOutcome {
        changed,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: true,
        preserve_changed: true,
    })
}

/// Collected changes for `--patch-text`, plus where the new side of a binary file lives
/// (collection keeps no bytes for binary files).
type PatchChanges = (Vec<FileChange>, PatchSide);

fn emit_patch_text_to_stdout(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    changes: Option<PatchChanges>,
    diff_options: DiffOptions,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let patch_opts = PatchTextOptions::from_repo(repo, diff_options)?;
    let mut stdout = std::io::stdout();
    if let Some((changes, binary_new)) = changes {
        if entries.len() != changes.len() {
            anyhow::bail!("internal error: patch-text entry/change count mismatch");
        }
        let resolved: Vec<PatchResolvedSide> = entries
            .into_iter()
            .zip(changes)
            .map(|(entry, fc)| {
                let (old, new) = if fc.binary {
                    (PatchSide::Blob, binary_new.clone())
                } else {
                    (
                        PatchSide::Bytes(fc.old_bytes.data),
                        PatchSide::Bytes(fc.new_bytes.data),
                    )
                };
                PatchResolvedSide { entry, old, new }
            })
            .collect();
        write_patch_resolved(repo, &resolved, &patch_opts, &mut stdout)?;
    } else {
        write_patch(repo, &entries, &patch_opts, &mut stdout)?;
    }
    Ok(())
}

/// Resolved inputs for deferred `grit show --patch-text` rendering.
pub struct ShowLogPatchTextParams {
    pub oid: ObjectId,
    pub format: DiffFormatArgs,
    pub pathspecs: Vec<String>,
    pub diff_merges: DiffMerges,
}

/// Build [`LogPatchOptions`] for git-apply-compatible `grit show` / `grit log --patch-text`.
pub fn log_patch_options_for_patch_text(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    pathspecs: &[String],
    diff_merges: DiffMerges,
) -> Result<LogPatchOptions> {
    let config = repo.config().context("could not load config")?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    format.apply_context(&mut diff_options);
    Ok(LogPatchOptions {
        diff: diff_options,
        abbrev_len: 7,
        commit_header: LogPatchCommitHeader::Full,
        binary_format: grit_lib::diff::LogPatchBinaryFormat::GitApplyBinaryPatch,
        pathspecs: pathspecs.to_vec(),
        diff_merges,
    })
}

fn map_log_patch_error(err: grit_lib::error::Error) -> anyhow::Error {
    anyhow::Error::new(err)
}

/// Outcome after streaming a show/log `--patch-text` body to stdout.
#[must_use]
pub fn streamed_log_patch_text_outcome(format: DiffFormatArgs) -> DiffOutcome {
    DiffOutcome {
        changed: true,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: true,
    }
}

/// Stream one commit in `git show --patch-text` shape.
pub fn emit_show_log_patch_text(
    repo: &grit_lib::repo::Repository,
    params: &ShowLogPatchTextParams,
    mut out: &mut dyn Write,
) -> Result<()> {
    let opts = log_patch_options_for_patch_text(
        repo,
        params.format,
        &params.pathspecs,
        params.diff_merges,
    )?;
    repo.odb()
        .with_pack_read_context(|| write_commit_log_patch(repo, &mut out, &params.oid, &opts))
        .map_err(map_log_patch_error)
}

/// Stream many commits in `git log --patch-text` shape.
pub fn emit_commits_log_patch_text(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    format: DiffFormatArgs,
    pathspecs: &[String],
    mut out: &mut dyn Write,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let opts = log_patch_options_for_patch_text(repo, format, pathspecs, DiffMerges::Combined)?;
    repo.odb()
        .with_pack_read_context(|| write_commits_log_patch(repo, &mut out, oids, &opts))
        .map_err(map_log_patch_error)
}

fn patch_text_outcome(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    changes: Option<PatchChanges>,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    quiet: bool,
) -> Result<DiffOutcome> {
    let changed = !entries.is_empty();
    emit_patch_text_to_stdout(repo, entries, changes, diff_options, quiet)?;
    Ok(DiffOutcome {
        changed,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: true,
    })
}

fn run_patch_text(opts: DiffRunOptions) -> Result<DiffOutcome> {
    let quiet = opts.quiet;
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let DiffArguments { target, pathspecs } = resolve_diff_target(&repo, &opts)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    let format = opts.format;
    let (entries, changes) = match target {
        DiffTarget::IndexVsHead => {
            let entries =
                staged_entries(&uncommitted_diff(&repo)?, &repo, &diff_options, &pathspecs)?;
            let changes = load_object_changes(&repo, &entries)?;
            (entries, Some((changes, PatchSide::Blob)))
        }
        DiffTarget::WorktreeVsIndex => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = unstaged_entries(&mut uncommitted, &repo, &diff_options, &pathspecs)?;
            let changes = uncommitted.load_worktree_changes(entries.clone())?;
            (entries, Some((changes, PatchSide::Worktree)))
        }
        DiffTarget::WorktreeVsHead | DiffTarget::WorktreeVsTree(_) => {
            let base_tree = worktree_base_tree(&repo, &target)?;
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = worktree_entries(
                &mut uncommitted,
                &repo,
                base_tree.as_ref(),
                &diff_options,
                &pathspecs,
            )?;
            let changes = uncommitted.load_worktree_changes(entries.clone())?;
            (entries, Some((changes, PatchSide::Worktree)))
        }
        _ => (
            target_tree_entries(
                &repo,
                &target,
                &pathspecs,
                &diff_options,
                &config,
                &thread_policy,
            )?,
            None,
        ),
    };
    patch_text_outcome(&repo, entries, changes, diff_options, format, quiet)
}

pub fn run(opts: DiffRunOptions) -> Result<DiffOutcome> {
    if opts.no_index {
        if opts.format.patch_text {
            return run_no_index_patch_text(opts);
        }
        return run_no_index(opts);
    }
    if opts.format.patch_text {
        return run_patch_text(opts);
    }
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let DiffArguments { target, pathspecs } = resolve_diff_target(&repo, &opts)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = opts.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    let outcome = match target {
        DiffTarget::IndexVsHead => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = staged_entries(&uncommitted, &repo, &diff_options, &pathspecs)?;
            match detail {
                DiffDetail::Stat | DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    drop(uncommitted);
                    outcome_from_entries(&repo, entries, detail, diff_options, format)?
                }
                _ => {
                    let staged = load_object_changes(&repo, &entries)?;
                    let peer = if opts.emit_json_hunk_ids {
                        peer_unstaged_changes_for_hunk_ids(
                            &mut uncommitted,
                            &repo,
                            &diff_options,
                            &pathspecs,
                            staged.iter().map(|fc| fc.path.as_str()),
                        )
                    } else {
                        Vec::new()
                    };
                    drop(uncommitted);
                    outcome_from_changes(
                        staged,
                        peer,
                        &config,
                        &thread_policy,
                        diff_options,
                        format,
                        DiffSide::Staged,
                        opts.emit_json_hunk_ids,
                    )
                }
            }
        }
        DiffTarget::WorktreeVsIndex => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = unstaged_entries(&mut uncommitted, &repo, &diff_options, &pathspecs)?;
            match detail {
                DiffDetail::Stat => {
                    let changes = uncommitted.load_worktree_changes(entries)?;
                    drop(uncommitted);
                    outcome_stat_from_changes(changes, diff_options, format)
                }
                DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    drop(uncommitted);
                    outcome_from_entries(&repo, entries, detail, diff_options, format)?
                }
                _ => {
                    let unstaged = uncommitted.load_worktree_changes(entries)?;
                    let peer = if opts.emit_json_hunk_ids {
                        peer_staged_changes_for_hunk_ids(
                            &uncommitted,
                            &repo,
                            &diff_options,
                            &pathspecs,
                            unstaged.iter().map(|fc| fc.path.as_str()),
                        )
                    } else {
                        Vec::new()
                    };
                    drop(uncommitted);
                    outcome_from_changes(
                        unstaged,
                        peer,
                        &config,
                        &thread_policy,
                        diff_options,
                        format,
                        DiffSide::Unstaged,
                        opts.emit_json_hunk_ids,
                    )
                }
            }
        }
        DiffTarget::WorktreeVsHead | DiffTarget::WorktreeVsTree(_) => {
            let base_tree = worktree_base_tree(&repo, &target)?;
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = worktree_entries(
                &mut uncommitted,
                &repo,
                base_tree.as_ref(),
                &diff_options,
                &pathspecs,
            )?;
            let changes = uncommitted.load_worktree_changes(entries)?;
            drop(uncommitted);
            match detail {
                DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    outcome_from_change_names(changes, format)
                }
                DiffDetail::Stat => outcome_stat_from_changes(changes, diff_options, format),
                _ => outcome_from_changes(
                    changes,
                    Vec::new(),
                    &config,
                    &thread_policy,
                    diff_options,
                    format,
                    DiffSide::Other,
                    false,
                ),
            }
        }
        _ => tree_diff_outcome(
            &repo,
            target_tree_entries(
                &repo,
                &target,
                &pathspecs,
                &diff_options,
                &config,
                &thread_policy,
            )?,
            detail,
            diff_options,
            format,
            opts.body_delivery,
        )?,
    };
    Ok(finish_outcome(&repo, format, outcome))
}

/// Resolve `grit diff` flags and operands into what to compare and the path limits.
fn resolve_diff_target(
    repo: &grit_lib::repo::Repository,
    opts: &DiffRunOptions,
) -> Result<DiffArguments> {
    let request = DiffRequest {
        staged: opts.staged,
        unstaged: opts.unstaged,
        against: opts.against.clone(),
        positionals: opts.commits.clone(),
        paths: opts.pathspecs.paths.clone(),
        pathspec_file: opts.pathspecs.pathspec_file_entries()?,
    };
    let cwd_prefix = pathspec_prefix_for_repo(repo)?;
    resolve_diff_request(repo, cwd_prefix.as_deref(), &request).map_err(map_diff_arguments_error)
}

/// Keep library failures downcastable to [`grit_lib::error::Error`] so revision messages, error
/// hints and `--json` kinds see them.
pub(crate) fn map_diff_arguments_error(err: ResolveDiffArgumentsError) -> anyhow::Error {
    match err {
        ResolveDiffArgumentsError::Library(err) => (*err).into(),
        ResolveDiffArgumentsError::UnresolvedRevision { spec, source } => {
            anyhow::Error::new(*source).context(format!("could not resolve '{spec}'"))
        }
        other => other.into(),
    }
}

/// Tree a working-tree target compares against: `HEAD`'s (none when unborn) or `--against`'s.
fn worktree_base_tree(
    repo: &grit_lib::repo::Repository,
    target: &DiffTarget,
) -> Result<Option<ObjectId>> {
    if let DiffTarget::WorktreeVsTree(tree) = target {
        return Ok(Some(*tree));
    }
    let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
    head.oid()
        .map(|oid| context::commit_tree(repo, oid))
        .transpose()
}

/// Path-limited tree entries for a commit or commit-range target.
fn target_tree_entries(
    repo: &grit_lib::repo::Repository,
    target: &DiffTarget,
    pathspecs: &[String],
    diff_options: &DiffOptions,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let entries = match target {
        DiffTarget::Commit(oid)
        | DiffTarget::Trees {
            old: None,
            new: oid,
        } => commit_entries(repo, oid, diff_options, config, thread_policy)?,
        DiffTarget::Trees {
            old: Some(base),
            new,
        } => tree_commit_entries(repo, base, new, diff_options, config, thread_policy)?,
        other => anyhow::bail!("unsupported diff target {other:?}"),
    };
    filter_diff_entries(entries, repo, pathspecs)
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
/// Options for diffing a single commit's patch (first parent vs commit tree).
pub struct CommitDiffOptions {
    /// Pathspecs from trailing `PATH` arguments (after `--`).
    pub paths: Vec<String>,
    pub format: DiffFormatArgs,
    pub word_level_inline: bool,
    /// Merge commit parent diff mode (`first-parent` or `combined`).
    pub diff_merges: DiffMerges,
}

/// Diff the change one commit introduced (first-parent tree vs commit tree).
pub fn diff_for_commit(
    repo: &grit_lib::repo::Repository,
    oid: &ObjectId,
    opts: CommitDiffOptions,
) -> Result<DiffOutcome> {
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let pathspecs = resolve_commit_diff_pathspecs(repo, &opts.paths)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options.word_level_inline = opts.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    let output = commit_diff_files(
        repo,
        oid,
        opts.diff_merges,
        detail,
        &diff_options,
        &pathspecs,
        config.as_ref(),
        &thread_policy,
    )?;
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_lib(output, format, detail),
    ))
}

/// Build a full patch-style [`DiffOutcome`] from tree diff entries (e.g. stash W vs base).
pub fn outcome_from_tree_entries(
    repo: &grit_lib::repo::Repository,
    mut entries: Vec<DiffEntry>,
) -> Result<DiffOutcome> {
    sort_entries_by_path(&mut entries);
    let changes = load_object_changes(repo, &entries)?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = false;
    let format = DiffFormatArgs::default();
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_changes(
            changes,
            Vec::new(),
            &config,
            &thread_policy,
            diff_options,
            format,
            DiffSide::Other,
            false,
        ),
    ))
}

fn outcome_from_tree_structured(
    items: Vec<TreeDiffStructured>,
    format: DiffFormatArgs,
) -> DiffOutcome {
    let files: Vec<FileDiff> = items
        .into_iter()
        .map(file_diff_from_tree_structured)
        .collect();
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn file_diff_from_lib(f: grit_lib::diff::FileDiff, detail: DiffDetail) -> FileDiff {
    let stat = if f.binary {
        None
    } else if detail == DiffDetail::Stat {
        Some(LineStat::from_hunks(&f.hunks).into())
    } else {
        None
    };
    let hunks = if detail == DiffDetail::Patch {
        Some(hunk_assign_ids(
            DiffSide::Other,
            &f.path,
            f.hunks,
            &[],
            false,
        ))
    } else {
        None
    };
    FileDiff {
        path: f.path,
        old_path: f.old_path,
        status: f.status.into(),
        old_mode: f.old_mode,
        new_mode: f.new_mode,
        old_encoding_lossy: f.old_encoding_lossy,
        new_encoding_lossy: f.new_encoding_lossy,
        binary: f.binary,
        content_changed: f.content_changed,
        similarity: None,
        stat,
        combined_status: None,
        hunks,
    }
}

fn file_diff_from_tree_structured(t: TreeDiffStructured) -> FileDiff {
    let e = t.entry;
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    let similarity = e.rename_similarity();
    let old_path = e
        .old_path
        .as_ref()
        .filter(|op| op.as_str() != path)
        .cloned();
    let structured = t.structured;
    let hunks = hunk_assign_ids(DiffSide::Other, &path, structured.hunks, &[], false);
    FileDiff {
        path,
        old_path,
        status: e.status.into(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: t.binary,
        content_changed: e.old_oid != e.new_oid,
        similarity,
        stat: None,
        combined_status: None,
        hunks: Some(hunks),
    }
}

fn hunk_assign_ids(
    side: DiffSide,
    path: &str,
    hunks: Vec<grit_lib::diff::structured::StructuredHunk>,
    peer_hunks: &[grit_lib::diff::structured::StructuredHunk],
    assign_hunk_ids: bool,
) -> Vec<Hunk> {
    let assign = assign_hunk_ids && matches!(side, DiffSide::Unstaged | DiffSide::Staged);
    let ids = if assign {
        hunk_ids_with_duplicate_peers(side, path, &hunks, peer_hunks)
    } else {
        Vec::new()
    };
    hunks
        .into_iter()
        .enumerate()
        .map(|(index, h)| {
            let mut out = hunk_from_structured(h);
            if assign {
                out.id = Some(ids[index].to_string());
                out.index = Some(index);
            }
            out
        })
        .collect()
}

fn hunk_from_structured(h: grit_lib::diff::structured::StructuredHunk) -> Hunk {
    Hunk {
        old_start: h.old_start,
        new_start: h.new_start,
        old_lines: h.old_lines,
        new_lines: h.new_lines,
        context: h.context,
        combined_header: None,
        id: None,
        index: None,
        lines: h
            .lines
            .into_iter()
            .map(|l| Line {
                kind: match l.kind {
                    StructuredLineKind::Context => LineKind::Context,
                    StructuredLineKind::Add => LineKind::Add,
                    StructuredLineKind::Del => LineKind::Del,
                },
                old: l.old,
                new: l.new,
                segments: l
                    .segments
                    .into_iter()
                    .map(|s| Segment {
                        text: s.text,
                        emphasis: s.emphasis,
                    })
                    .collect(),
                no_newline_at_eof: l.no_newline_at_eof,
                parents: None,
            })
            .collect(),
    }
}

fn diff_entry_display_path(e: &DiffEntry) -> &str {
    e.new_path
        .as_deref()
        .or(e.old_path.as_deref())
        .unwrap_or("")
}

fn diff_entry_matches_primary(e: &DiffEntry, primary: &std::collections::HashSet<&str>) -> bool {
    peer_scope_includes_path(diff_entry_display_path(e), e.old_path.as_deref(), primary)
}

fn filter_entries_to_primary_paths(
    entries: Vec<DiffEntry>,
    primary: &std::collections::HashSet<&str>,
) -> Vec<DiffEntry> {
    entries
        .into_iter()
        .filter(|e| diff_entry_matches_primary(e, primary))
        .collect()
}

fn uncommitted_diff(repo: &grit_lib::repo::Repository) -> Result<UncommittedDiff<'_>> {
    repo.diff()
        .uncommitted()
        .context("could not load the index")
}

/// Staged diff entries (index vs `HEAD`'s tree) within `pathspecs`, without loading blobs.
fn staged_entries(
    uncommitted: &UncommittedDiff<'_>,
    repo: &grit_lib::repo::Repository,
    diff_options: &DiffOptions,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    let entries = uncommitted
        .staged_entries(diff_options.renames)
        .context("could not diff the index against HEAD")?;
    filter_diff_entries(entries, repo, pathspecs)
}

/// Unstaged diff entries (index vs working tree) within `pathspecs`, without loading blobs.
fn unstaged_entries(
    uncommitted: &mut UncommittedDiff<'_>,
    repo: &grit_lib::repo::Repository,
    diff_options: &DiffOptions,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    let entries = uncommitted
        .unstaged_entries(diff_options.renames)
        .context("could not diff the index against the working tree")?;
    filter_diff_entries(entries, repo, pathspecs)
}

/// Working tree and index against `base_tree`, within `pathspecs`, without loading blobs.
fn worktree_entries(
    uncommitted: &mut UncommittedDiff<'_>,
    repo: &grit_lib::repo::Repository,
    base_tree: Option<&ObjectId>,
    diff_options: &DiffOptions,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    let entries = uncommitted
        .worktree_entries(base_tree, diff_options.renames)
        .context("could not diff the working tree")?;
    filter_diff_entries(entries, repo, pathspecs)
}

fn peer_unstaged_changes_for_hunk_ids<'a>(
    uncommitted: &mut UncommittedDiff<'_>,
    repo: &grit_lib::repo::Repository,
    diff_options: &DiffOptions,
    pathspecs: &[String],
    primary_paths: impl IntoIterator<Item = &'a str>,
) -> Vec<FileChange> {
    use std::collections::HashSet;

    let primary: HashSet<&str> = primary_paths.into_iter().collect();
    if primary.is_empty() || repo.work_tree().is_none() {
        return Vec::new();
    }
    let Ok(entries) = unstaged_entries(uncommitted, repo, diff_options, pathspecs) else {
        return Vec::new();
    };
    let entries = filter_entries_to_primary_paths(entries, &primary);
    uncommitted
        .load_worktree_changes(entries)
        .unwrap_or_default()
}

fn peer_staged_changes_for_hunk_ids<'a>(
    uncommitted: &UncommittedDiff<'_>,
    repo: &grit_lib::repo::Repository,
    diff_options: &DiffOptions,
    pathspecs: &[String],
    primary_paths: impl IntoIterator<Item = &'a str>,
) -> Vec<FileChange> {
    use std::collections::HashSet;

    let primary: HashSet<&str> = primary_paths.into_iter().collect();
    if primary.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = staged_entries(uncommitted, repo, diff_options, pathspecs) else {
        return Vec::new();
    };
    let entries = filter_entries_to_primary_paths(entries, &primary);
    load_object_changes(repo, &entries).unwrap_or_default()
}

#[expect(clippy::too_many_arguments)]
fn outcome_from_changes(
    changes: Vec<FileChange>,
    peer_changes: Vec<FileChange>,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    side: DiffSide,
    assign_hunk_ids: bool,
) -> DiffOutcome {
    use std::collections::HashSet;

    let diff_options = diff_options.cap_word_level_inline_for_changed_files(changes.len());
    let primary_paths: HashSet<&str> = changes.iter().map(|fc| fc.path.as_str()).collect();
    let peer_hunks_by_path =
        if assign_hunk_ids && matches!(side, DiffSide::Unstaged | DiffSide::Staged) {
            let peer_pairs = peer_changes.into_iter().map(|fc| {
                let structured = fc.structured(diff_options.clone());
                (fc.path, structured.hunks)
            });
            peer_hunks_by_primary_path(&primary_paths, peer_pairs)
        } else {
            std::collections::HashMap::new()
        };
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = 0;
    let est_bytes = changes.len().saturating_mul(8192);
    use grit_lib::parallel::par_map;
    let files: Vec<FileDiff> = par_map(&changes, par_opts, est_bytes, |fc| {
        let peer_hunks = peer_hunks_by_path
            .get(&fc.path)
            .map(|h| h.as_slice())
            .unwrap_or(&[]);
        build_file_diff(fc, diff_options.clone(), side, peer_hunks, assign_hunk_ids)
    })
    .into_iter()
    .filter(|f| {
        let ws = diff_options.whitespace;
        if !ws.ignore_all_space && !ws.ignore_space_change && !ws.ignore_blank_lines {
            return true;
        }
        if f.old_path.is_some() && matches!(f.status, ChangeStatus::Renamed | ChangeStatus::Copied)
        {
            return true;
        }
        if f.binary {
            return f.content_changed;
        }
        !matches!(&f.hunks, Some(h) if h.is_empty())
    })
    .collect();
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn pathspec_prefix_for_repo(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    Ok(pathdiff(&cwd, work_tree))
}

/// Commit and range pathspecs, resolved relative to the current directory.
pub(crate) fn resolve_commit_diff_pathspecs(
    repo: &grit_lib::repo::Repository,
    raw_paths: &[String],
) -> Result<Vec<String>> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let cwd_prefix = pathspec_prefix_for_repo(repo)?;
    grit_lib::diff::resolve_commit_diff_pathspecs(repo, cwd_prefix.as_deref(), raw_paths)
        .map_err(map_diff_arguments_error)
}

fn filter_diff_entries(
    entries: Vec<DiffEntry>,
    repo: &grit_lib::repo::Repository,
    specs: &[String],
) -> Result<Vec<DiffEntry>> {
    let matcher = repo.pathspec_matcher(specs)?;
    Ok(entries
        .into_iter()
        .filter(|e| {
            let path = e
                .new_path
                .as_deref()
                .or(e.old_path.as_deref())
                .unwrap_or("");
            matcher.matches_change(path, e.old_path.as_deref())
        })
        .collect())
}

fn outcome_stat_from_changes(
    changes: Vec<FileChange>,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
) -> DiffOutcome {
    let mut lines = LineStat::default();
    let files = changes
        .into_iter()
        .map(|fc| {
            let stat = fc.line_stat(&diff_options);
            if let Some(counts) = stat {
                lines += counts;
            }
            let stat = stat.map(LineStatJson::from);
            FileDiff {
                path: fc.path,
                old_path: fc.old_path,
                status: fc.status.into(),
                old_mode: fc.old_mode.octal_if_present(),
                new_mode: fc.new_mode.octal_if_present(),
                old_encoding_lossy: fc.old_bytes.encoding_lossy,
                new_encoding_lossy: fc.new_bytes.encoding_lossy,
                binary: fc.binary,
                content_changed: fc.content_changed,
                similarity: fc.similarity,
                stat,
                combined_status: None,
                hunks: None,
            }
        })
        .collect::<Vec<_>>();
    let files_changed = files.len();
    DiffOutcome {
        changed: files_changed > 0,
        totals: Some(DiffTotalsJson {
            files_changed,
            insertions: lines.insertions,
            deletions: lines.deletions,
        }),
        files,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn tree_commit_entries(
    repo: &grit_lib::repo::Repository,
    base: &ObjectId,
    tip: &ObjectId,
    diff_options: &DiffOptions,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        base,
        tip,
        diff_options.rename_threshold_u32(),
        config,
        thread_policy,
    )
    .context("could not diff commits")?;
    entries = apply_diff_filter(entries, diff_options.filter);
    sort_entries_by_path(&mut entries);
    Ok(entries)
}

fn commit_entries(
    repo: &grit_lib::repo::Repository,
    oid: &ObjectId,
    diff_options: &DiffOptions,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let commit = context::read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(context::commit_tree(repo, parent)?),
        None => None,
    };
    let mut entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        old_tree.as_ref(),
        Some(&new_tree),
        "",
        diff_options.rename_threshold_u32(),
        config,
        thread_policy,
    )
    .context("could not diff the commit")?;
    sort_entries_by_path(&mut entries);
    Ok(entries)
}

fn tree_diff_outcome(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    detail: DiffDetail,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    body_delivery: DiffBodyDelivery,
) -> Result<DiffOutcome> {
    if detail == DiffDetail::Patch {
        match body_delivery {
            DiffBodyDelivery::MetadataOnly => Ok(DiffOutcome {
                changed: !entries.is_empty(),
                files: Vec::new(),
                totals: None,
                format,
                path_display: None,
                body_streamed: false,
                preserve_changed: true,
            }),
            DiffBodyDelivery::Stream(mode) => {
                stream_tree_patch_diff(repo, &entries, diff_options, format, mode)
            }
            DiffBodyDelivery::Collect => {
                let config = repo.config().context("could not load config")?;
                let thread_policy = repo.thread_policy();
                let structured = tree_diff_structured_parallel(
                    repo.odb(),
                    entries,
                    config.as_ref(),
                    &thread_policy,
                    diff_options,
                )?;
                Ok(outcome_from_tree_structured(structured, format))
            }
        }
    } else {
        outcome_from_entries(repo, entries, detail, diff_options, format)
    }
}

fn stream_tree_patch_diff(
    repo: &grit_lib::repo::Repository,
    entries: &[DiffEntry],
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    mode: OutputMode,
) -> Result<DiffOutcome> {
    let changed = !entries.is_empty();
    if !changed {
        return Ok(DiffOutcome {
            changed: false,
            files: Vec::new(),
            totals: None,
            format,
            path_display: None,
            body_streamed: false,
            preserve_changed: false,
        });
    }
    let path_display = PathDisplayOptions::from_repo(repo).ok();
    let color = diff_render::use_color();
    let mut stdout = std::io::stdout();
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let mut index = 0usize;
    for_each_tree_diff_structured(
        repo.odb(),
        entries,
        diff_options,
        config.as_ref(),
        &thread_policy,
        |structured| {
            let file = file_diff_from_tree_structured(structured);
            let map_cli_err = |e: anyhow::Error| grit_lib::error::Error::PathError(e.to_string());
            match mode {
                OutputMode::Human => render_file(&mut stdout, &file, color, path_display.as_ref())
                    .map_err(map_cli_err)?,
                OutputMode::Markdown => {
                    markdown::print_diff_file(&mut stdout, &file, path_display.as_ref(), index > 0)
                        .map_err(map_cli_err)?
                }
                OutputMode::Json => unreachable!("stream delivery is not used for JSON"),
            }
            index += 1;
            Ok(())
        },
    )
    .context("structured diff")?;
    Ok(DiffOutcome {
        changed: true,
        files: Vec::new(),
        totals: None,
        format,
        path_display,
        body_streamed: true,
        preserve_changed: true,
    })
}

fn build_file_diff(
    fc: &FileChange,
    diff_options: DiffOptions,
    side: DiffSide,
    peer_hunks: &[grit_lib::diff::structured::StructuredHunk],
    assign_hunk_ids: bool,
) -> FileDiff {
    let structured = fc.structured(diff_options);
    let hunks = hunk_assign_ids(
        side,
        &fc.path,
        structured.hunks,
        peer_hunks,
        assign_hunk_ids,
    );

    FileDiff {
        path: fc.path.clone(),
        old_path: fc.old_path.clone(),
        status: fc.status.into(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: fc.binary,
        content_changed: fc.content_changed,
        similarity: fc.similarity,
        stat: None,
        combined_status: None,
        hunks: Some(hunks),
    }
}

// --- Human (delta-style) rendering -----------------------------------------

impl DiffOutcome {
    #[must_use]
    pub(crate) fn format_flags(&self) -> DiffFormatArgs {
        self.format
    }

    pub(crate) fn path_display_opts(&self) -> Option<&PathDisplayOptions> {
        self.path_display.as_ref()
    }

    /// Render only the diff body (no commit header). `empty_message` is printed when there
    /// are no files in patch mode; pass `None` to stay silent (as `grit show` does).
    pub fn render_human_diff(&self, w: &mut dyn Write, empty_message: Option<&str>) -> Result<()> {
        if self.body_streamed {
            if !self.changed {
                if let Some(msg) = empty_message {
                    outln!(w, "{msg}");
                }
            }
            return Ok(());
        }
        if self.files.is_empty() {
            if format_is_byte_list_or_stat(self.format) {
                return Ok(());
            }
            if let Some(msg) = empty_message {
                outln!(w, "{msg}");
            }
            return Ok(());
        }
        let color = diff_render::use_color();
        let fmt = self.format;
        if fmt.human_numstat() {
            return render_numstat(w, &self.files, self.path_display.as_ref());
        }
        if fmt.human_name_only() {
            return render_name_only(w, &self.files, self.path_display.as_ref());
        }
        if fmt.human_name_status() {
            return render_name_status(w, &self.files, self.path_display.as_ref());
        }
        if fmt.human_shortstat_only() {
            let stat = diffstat_from_files(&self.files);
            return render_shortstat(w, &stat);
        }
        if fmt.human_stat_block() {
            let stat = diffstat_from_files(&self.files);
            return render_stat(w, &stat, color, self.path_display.as_ref());
        }
        for file in &self.files {
            render_file(w, file, color, self.path_display.as_ref())?;
        }
        Ok(())
    }

    /// Markdown diff body only (used by `grit show` after the commit header).
    pub fn render_markdown_diff(&self, w: &mut dyn Write) -> Result<()> {
        if self.body_streamed {
            if !self.changed {
                outln!(w, "No changes.");
            }
            return Ok(());
        }
        if self.files.is_empty() && format_is_byte_list_or_stat(self.format) {
            return Ok(());
        }
        let fmt = self.format;
        if fmt.human_shortstat_only() {
            let stat = diffstat_from_files(&self.files);
            outln!(w, "{}", diff_render::summary_line(stat.lib().totals()));
            return Ok(());
        }
        if fmt.detail() == DiffDetail::Stat {
            let stat = diffstat_from_files(&self.files);
            return diff_render::render_markdown_stat(w, &stat, self.path_display.as_ref());
        }
        if fmt.human_name_status() {
            for file in &self.files {
                let mut buf = Vec::new();
                render_name_status(
                    &mut buf,
                    std::slice::from_ref(file),
                    self.path_display.as_ref(),
                )?;
                let line = String::from_utf8(buf).unwrap_or_default();
                outln!(w, "- `{}`", line.trim());
            }
            return Ok(());
        }
        if fmt.human_name_only() {
            for file in &self.files {
                outln!(w, "- `{}`", file.path);
            }
            return Ok(());
        }
        markdown::print_diff_files(w, &self.files, self.path_display.as_ref())?;
        Ok(())
    }
}

impl HumanRender for DiffOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        self.render_human_diff(w, Some("No changes."))
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        self.render_markdown_diff(w)
    }
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Status word aligned with [`ChangeStatus`] JSON (`--json` / `--markdown` metadata).
#[must_use]
pub(crate) fn file_diff_status_label(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "added",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Modified => "modified",
        ChangeStatus::TypeChanged => "type_changed",
        ChangeStatus::Renamed => "renamed",
        ChangeStatus::Copied => "copied",
        ChangeStatus::Unmerged => "unmerged",
    }
}

pub(crate) fn write_file_diff_metadata_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    match file.status {
        ChangeStatus::Deleted => outln!(w, "deleted"),
        ChangeStatus::Renamed => outln!(w, "renamed"),
        _ => {}
    }
    write_file_diff_mode_lines_human(w, file)
}

fn write_file_diff_mode_lines_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            outln!(w, "old mode {m}");
        }
        if let Some(m) = &file.new_mode {
            outln!(w, "new mode {m}");
        }
    }
    Ok(())
}

fn render_file(
    w: &mut dyn Write,
    file: &FileDiff,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    outln!(w);
    let header = ui::format_diff_file_header(file.old_path.as_deref(), &file.path, path_opts);
    outln!(w, "{}", paint(color, "1;4", &header)); // bold + underline

    write_file_diff_metadata_human(w, file)?;

    if file.binary {
        if file.content_changed {
            outln!(w, "{}", paint(color, FG_DIM, "Binary file differs"));
        }
        return Ok(());
    }

    // Width of each line-number column = widest number shown.
    let hunks = file.hunks.as_deref().unwrap_or(&[]);
    let width = hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .flat_map(|l| [l.old, l.new])
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len())
        .max(2);

    for hunk in hunks {
        render_hunk(w, hunk, width, color)?;
    }
    Ok(())
}

fn render_hunk(w: &mut dyn Write, hunk: &Hunk, width: usize, color: bool) -> Result<()> {
    if let Some(header) = &hunk.combined_header {
        outln!(w, "{header}");
        for line in &hunk.lines {
            let mut prefix = String::new();
            if let Some(parents) = &line.parents {
                for p in parents {
                    prefix.push_str(p);
                }
            }
            let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
            outln!(w, "{prefix}{text}");
        }
        return Ok(());
    }
    let old_rng = format_hunk_range(hunk.old_start, hunk.old_lines);
    let new_rng = format_hunk_range(hunk.new_start, hunk.new_lines);
    let range = format!("@@ -{old_rng} +{new_rng} @@");
    match (&hunk.context, color) {
        (Some(ctx), true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range} {ctx}")));
        }
        (Some(ctx), false) => {
            outln!(w, "{range} {ctx}");
        }
        (None, false) => {
            outln!(w, "{range}");
        }
        (None, true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range}")));
        }
    }

    for line in &hunk.lines {
        outln!(w, "{}", render_line(line, width, color));
        if line.no_newline_at_eof {
            let marker = "\\ No newline at end of file";
            outln!(w, "{}", paint(color, FG_DIM, marker));
        }
    }
    Ok(())
}

fn render_line(line: &Line, width: usize, color: bool) -> String {
    let (base_bg, emph_bg, num_code, sign) = match line.kind {
        LineKind::Del => (BG_DEL, BG_DEL_EMPH, FG_DEL_NUM, '-'),
        LineKind::Add => (BG_ADD, BG_ADD_EMPH, FG_ADD_NUM, '+'),
        LineKind::Context => ("", "", FG_DIM, ' '),
    };

    // Gutter: two right-aligned line-number columns (changed side colored).
    let old_col = num_cell(line.old, width, num_code, line.kind == LineKind::Del, color);
    let new_col = num_cell(line.new, width, num_code, line.kind == LineKind::Add, color);
    let bar = paint(color, FG_DIM, "│");
    let gutter = format!("{old_col} {new_col} {bar} ");

    if !color {
        // Plain text: lead with a +/-/space marker so piped output is readable.
        let raw: String = line.segments.iter().map(|s| s.text.as_str()).collect();
        return format!("{gutter}{sign} {}", expand_tabs(&raw, 0));
    }

    // Colored: paint the background behind the text only (brighter for the
    // emphasized word-level changes). We deliberately do NOT pad to a fixed
    // width — padding past the terminal edge would wrap onto extra rows and
    // render those as blank colored bands.
    let mut body = String::new();
    let mut col = 0;
    for seg in &line.segments {
        let text = expand_tabs(&seg.text, col);
        col += text.chars().count();
        if base_bg.is_empty() {
            body.push_str(&text);
        } else {
            // Pair the dark background with an explicit light foreground so the
            // text is legible regardless of the terminal's theme.
            let bg = if seg.emphasis { emph_bg } else { base_bg };
            body.push_str(&format!("\x1b[{FG_ON_DIFF};{bg}m{text}"));
        }
    }
    if !base_bg.is_empty() {
        body.push_str(RESET);
    }
    format!("{gutter}{body}")
}

/// Render one line-number cell, right-aligned to `width`, colored when the line
/// is the changed side; blank when the number is absent.
fn num_cell(n: Option<usize>, width: usize, code: &str, changed: bool, color: bool) -> String {
    match n {
        Some(n) => {
            let s = format!("{n:>width$}");
            if color {
                paint(true, if changed { code } else { FG_DIM }, &s)
            } else {
                s
            }
        }
        None => " ".repeat(width),
    }
}

/// Expand tabs to the next [`TAB_WIDTH`] stop, given the starting column.
fn expand_tabs(s: &str, start_col: usize) -> std::borrow::Cow<'_, str> {
    if !s.contains('\t') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + TAB_WIDTH);
    let mut col = start_col;
    for ch in s.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::expand_tabs;
    use std::borrow::Cow;

    #[test]
    fn expand_tabs_borrows_tab_free_text_and_aligns_tab_stops() {
        assert!(matches!(expand_tabs("plain ü text", 3), Cow::Borrowed(_)));
        assert_eq!(expand_tabs("\tx", 0), "        x");
        assert_eq!(expand_tabs("ab\tc", 0), "ab      c");
        assert_eq!(expand_tabs("ü\tc", 0), "ü       c");
        assert_eq!(expand_tabs("\t", 5), "   ");
        assert_eq!(expand_tabs("", 0), "");
    }

    mod stream_output_parity {
        use super::super::{file_diff_from_tree_structured, render_file, PathDisplayOptions};
        use crate::markdown;
        use crate::output::OutputMode;
        use grit_lib::diff::{
            diff_trees, for_each_tree_diff_structured, tree_diff_structured_parallel, write_patch,
            write_patch_structured, DiffEntry, DiffOptions, PatchTextOptions,
        };
        use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
        use grit_lib::repo::{InitOptions, Repository};

        fn repo_with_range() -> (tempfile::TempDir, Repository, Vec<DiffEntry>) {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
            let odb = repo.odb();
            let tree = |tag: &str| {
                let entries: Vec<TreeEntry> = (0..6)
                    .map(|i| {
                        let oid = odb
                            .write(
                                ObjectKind::Blob,
                                format!("{tag} {i}\nsecond line\n").as_bytes(),
                            )
                            .expect("blob");
                        TreeEntry {
                            mode: 0o100644,
                            name: format!("dir/f{i}.txt").into_bytes(),
                            oid,
                        }
                    })
                    .collect();
                odb.write(ObjectKind::Tree, &serialize_tree(&entries))
                    .expect("tree")
            };
            let old_tree = tree("old");
            let new_tree = tree("new");
            let entries =
                diff_trees(odb, Some(&old_tree), Some(&new_tree), "").expect("diff entries");
            (dir, repo, entries)
        }

        fn render_human_or_markdown(
            repo: &Repository,
            entries: &[DiffEntry],
            mode: OutputMode,
            stream: bool,
        ) -> Vec<u8> {
            let config = repo.config().expect("config");
            let thread_policy = repo.thread_policy();
            let diff_options = DiffOptions::default().with_word_level_inline(false);
            let path_display = PathDisplayOptions::from_repo(repo).ok();
            let mut buf = Vec::new();
            let mut render_one = |index: usize, structured| {
                let file = file_diff_from_tree_structured(structured);
                match mode {
                    OutputMode::Human => {
                        render_file(&mut buf, &file, false, path_display.as_ref()).expect("human")
                    }
                    OutputMode::Markdown => {
                        markdown::print_diff_file(&mut buf, &file, path_display.as_ref(), index > 0)
                            .expect("markdown")
                    }
                    OutputMode::Json => unreachable!(),
                }
            };
            if stream {
                let mut index = 0usize;
                for_each_tree_diff_structured(
                    repo.odb(),
                    entries,
                    diff_options,
                    config.as_ref(),
                    &thread_policy,
                    |structured| {
                        render_one(index, structured);
                        index += 1;
                        Ok(())
                    },
                )
                .expect("stream render");
            } else {
                let rows = tree_diff_structured_parallel(
                    repo.odb(),
                    entries.to_vec(),
                    config.as_ref(),
                    &thread_policy,
                    diff_options,
                )
                .expect("collect structured");
                for (index, row) in rows.into_iter().enumerate() {
                    render_one(index, row);
                }
            }
            buf
        }

        #[test]
        fn human_stream_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let collected = render_human_or_markdown(&repo, &entries, OutputMode::Human, false);
            let streamed = render_human_or_markdown(&repo, &entries, OutputMode::Human, true);
            assert_eq!(collected, streamed);
        }

        #[test]
        fn markdown_stream_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let collected = render_human_or_markdown(&repo, &entries, OutputMode::Markdown, false);
            let streamed = render_human_or_markdown(&repo, &entries, OutputMode::Markdown, true);
            assert_eq!(collected, streamed);
        }

        #[test]
        fn patch_text_pipeline_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let config = repo.config().expect("config");
            let thread_policy = repo.thread_policy();
            let diff_options = DiffOptions::default();
            let patch_opts =
                PatchTextOptions::from_repo(&repo, diff_options.clone()).expect("patch opts");
            let mut pipelined = Vec::new();
            write_patch(&repo, &entries, &patch_opts, &mut pipelined).expect("write_patch");
            let structured = tree_diff_structured_parallel(
                repo.odb(),
                entries,
                config.as_ref(),
                &thread_policy,
                diff_options,
            )
            .expect("structured");
            let mut collected = Vec::new();
            write_patch_structured(&repo, &structured, &patch_opts, &mut collected)
                .expect("write_patch_structured");
            assert_eq!(pipelined, collected);
        }
    }
}

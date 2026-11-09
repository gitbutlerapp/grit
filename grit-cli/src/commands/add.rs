//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::index::IndexFile::stage`] so the CLI stays
//! a thin wrapper over the library.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::apply::{apply_patch_to_index_and_write, IndexApplyOptions};
use grit_lib::diff::DiffSide;
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::add::{StageMode, StageOptions, StageOutcome};
use grit_lib::porcelain::hunk_select::{
    parse_hunk_id_list, HunkSelectError, LineRange, StageHunksOutcome,
};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::ProgressSink;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
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
    path_display: Option<PathDisplayOptions>,
}

impl AddOutcome {
    fn from_stage(outcome: StageOutcome, dry_run: bool, no_paths: bool) -> Self {
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

    fn from_hunks(outcome: StageHunksOutcome) -> Self {
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

    fn with_path_display(mut self, repo: &Repository) -> Self {
        self.path_display = PathDisplayOptions::from_repo(repo).ok();
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

/// Exit code for `--hunks` / `--lines` failures (unknown, stale, or wrong-side ids).
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err
        .downcast_ref::<grit_lib::error::Error>()
        .is_some_and(grit_lib::error::Error::is_cancelled)
    {
        return 130;
    }
    let selection = err.downcast_ref::<HunkSelectError>().or_else(|| match err
        .downcast_ref::<grit_lib::error::Error>()
    {
        Some(grit_lib::error::Error::Diff(grit_lib::error::DiffError::HunkSelection(sel))) => {
            Some(sel)
        }
        _ => None,
    });
    if let Some(sel) = selection {
        match sel {
            HunkSelectError::UnknownId { .. }
            | HunkSelectError::StaleId { .. }
            | HunkSelectError::WrongSide { .. }
            | HunkSelectError::UnselectableSide { .. } => 2,
            HunkSelectError::Binary { .. }
            | HunkSelectError::RangeOutOfBounds { .. }
            | HunkSelectError::RangeSelectsNothing { .. }
            | HunkSelectError::Gitlink { .. }
            | HunkSelectError::InvalidMode { .. }
            | HunkSelectError::MissingPath
            | HunkSelectError::Apply(_)
            | HunkSelectError::PatchBuild => 1,
            _ => 1,
        }
    } else {
        1
    }
}

/// Default unified context lines when staging hunks (matches `grit diff --json` without `-U`).
const HUNK_CONTEXT_LINES: usize = 3;

/// Parse `<path>:<start>-<end>` or `<path>:<line>` tokens from `--lines`.
pub fn parse_line_range_list(tokens: &[String]) -> Result<Vec<LineRange>> {
    let mut ranges = Vec::new();
    for token in tokens {
        for part in token.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            ranges.push(parse_line_range_token(part)?);
        }
    }
    Ok(ranges)
}

fn parse_line_range_token(token: &str) -> Result<LineRange> {
    let colon = token
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("invalid line range '{token}' (expected path:start-end)"))?;
    let path = token[..colon].trim();
    if path.is_empty() {
        bail!("invalid line range '{token}' (missing path before ':')");
    }
    let spec = token[colon + 1..].trim();
    let (start, end) = if let Some((a, b)) = spec.split_once('-') {
        let start: usize = a
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid line number in '{token}'"))?;
        let end: usize = b
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid line number in '{token}'"))?;
        (start, end)
    } else {
        let n: usize = spec
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid line number in '{token}'"))?;
        (n, n)
    };
    Ok(LineRange {
        path: path.to_string(),
        start,
        end,
    })
}

pub fn run_hunks(
    args: &PathspecArgs,
    hunks: &[String],
    lines: &[String],
    unified_context: Option<usize>,
    dry_run: bool,
) -> Result<AddOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    let mut selection = parse_hunk_id_list(hunks).map_err(map_hunk_error)?;
    selection.ranges = parse_line_range_list(lines)?;
    let mut diff_options = repo
        .index()
        .hunk_diff_options()
        .map_err(anyhow::Error::new)?;
    diff_options.context_lines = HUNK_CONTEXT_LINES;
    if let Some(n) = unified_context {
        diff_options.context_lines = n;
    }
    let outcome = repo
        .index()
        .stage_hunks(
            DiffSide::Unstaged,
            &selection,
            &pathspecs,
            diff_options,
            dry_run,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_hunk_error)?;
    Ok(AddOutcome::from_hunks(outcome).with_path_display(&repo))
}

/// Keep hunk-selection failures downcastable to [`HunkSelectError`] for the `--hunks` exit codes
/// and JSON kinds.
pub(crate) fn map_hunk_error(err: grit_lib::error::Error) -> anyhow::Error {
    match err {
        grit_lib::error::Error::Diff(grit_lib::error::DiffError::HunkSelection(sel)) => {
            anyhow::Error::new(sel)
        }
        other => anyhow::Error::new(other),
    }
}

pub fn run_patch_file(patch_file: &str, dry_run: bool) -> Result<AddOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let patches = crate::patch_selection::parse_patch_file(patch_file)?;
    let outcome = apply_patch_to_index_and_write(
        &repo,
        &patches,
        &IndexApplyOptions {
            dry_run,
            ..IndexApplyOptions::default()
        },
        &crate::interrupt::process_cancel_token(),
    )
    .map_err(map_apply_error)?;
    let count = outcome.added.len() + outcome.modified.len() + outcome.removed.len();
    Ok(AddOutcome {
        added: outcome.added,
        modified: outcome.modified,
        removed: outcome.removed,
        intent_to_add: Vec::new(),
        dry_run: outcome.dry_run,
        count,
        staged: count,
        staged_hunks: BTreeMap::new(),
        staged_lines: BTreeMap::new(),
        no_paths: false,
        embedded_gitlinks: Vec::new(),
        path_display: PathDisplayOptions::from_repo(&repo).ok(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    args: &PathspecArgs,
    update: bool,
    dry_run: bool,
    intent_to_add: bool,
    force: bool,
    executable: bool,
    non_executable: bool,
    progress: &mut dyn ProgressSink,
) -> Result<AddOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    if executable && non_executable {
        bail!("--executable and --non-executable cannot be used together");
    }
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let no_paths = resolved.is_entire_tree();
    if (executable || non_executable) && no_paths {
        bail!("--executable and --non-executable require at least one path");
    }
    if intent_to_add && no_paths {
        bail!("--intent-to-add requires at least one path");
    }
    if intent_to_add && update {
        bail!("--intent-to-add cannot be used with --update");
    }
    let outcome = stage_with_hint(
        &repo,
        resolved,
        if update {
            StageMode::Update
        } else {
            StageMode::All
        },
        dry_run,
        intent_to_add,
        force,
        if executable {
            StageExecutableHint::SetExecutable
        } else if non_executable {
            StageExecutableHint::SetNonExecutable
        } else {
            StageExecutableHint::None
        },
        progress,
        &crate::interrupt::process_cancel_token(),
    )?;
    Ok(AddOutcome::from_stage(outcome, dry_run, no_paths).with_path_display(&repo))
}

#[allow(clippy::too_many_arguments)]
fn stage_with_hint(
    repo: &Repository,
    resolved: grit_lib::worktree::pathspec::Pathspec,
    mode: StageMode,
    dry_run: bool,
    intent_to_add: bool,
    force_ignored: bool,
    executable_hint: StageExecutableHint,
    progress: &mut dyn ProgressSink,
    cancel: &grit_lib::progress::CancelToken,
) -> Result<StageOutcome> {
    let pathspecs = resolved.specs().to_vec();
    let pathspec_sources: Vec<String> = resolved.iter().map(str::to_owned).collect();
    let pathspec_sources = if pathspec_sources.is_empty() {
        pathspecs.clone()
    } else {
        pathspec_sources
    };
    #[allow(clippy::field_reassign_with_default)]
    let opts = {
        let mut o = StageOptions::default();
        o.pathspecs = pathspecs;
        o.pathspec_sources = pathspec_sources;
        o.mode = mode;
        o.executable_hint = executable_hint;
        o.dry_run = dry_run;
        o.intent_to_add = intent_to_add;
        o.force_ignored = force_ignored;
        o
    };
    repo.index()
        .stage(&opts, progress, cancel)
        .map_err(crate::error_map::map_stage_lib_error)
}

fn map_apply_error(err: grit_lib::error::ApplyError) -> anyhow::Error {
    if err == grit_lib::error::ApplyError::Cancelled {
        return crate::json_error::map_lib_error(LibError::Cancelled);
    }
    anyhow::anyhow!("{err}")
}

/// Dispatch parsed CLI args for `Add`.
pub fn dispatch(
    args: crate::args::AddArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, finish_hunk_error, CommandEnv};

    use crate::progress;

    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let AddArgs {
        pathspecs,
        update,
        dry_run,
        intent_to_add,
        force,
        executable,
        non_executable,
        patch_file,
        hunks,
        lines,
        unified_context,
    } = args;
    {
        if unified_context.is_some() && hunks.is_empty() && lines.is_empty() {
            bail!("-U / --unified requires --hunks or --lines");
        }
        if let Some(file) = patch_file {
            if !hunks.is_empty() || !lines.is_empty() {
                bail!("--patch-file cannot be used with --hunks or --lines");
            }
            if executable || non_executable {
                bail!("--patch-file cannot be used with --executable or --non-executable");
            }
            if force {
                bail!("--patch-file cannot be used with --force");
            }
            if update {
                bail!("--patch-file cannot be used with --update");
            }
            if intent_to_add {
                bail!("--patch-file cannot be used with --intent-to-add");
            }
            if !pathspecs.paths.is_empty() || pathspecs.pathspec_from_file.is_some() {
                bail!("--patch-file cannot be used with path arguments");
            }
            emit_success(commands::add::run_patch_file(&file, dry_run)?, opts, ctx)
        } else if !hunks.is_empty() || !lines.is_empty() {
            if executable || non_executable {
                bail!("--hunks and --lines cannot be used with --executable or --non-executable");
            }
            if force {
                bail!("--hunks and --lines cannot be used with --force");
            }
            if update {
                bail!("--hunks and --lines cannot be used with --update");
            }
            if intent_to_add {
                bail!("--hunks and --lines cannot be used with --intent-to-add");
            }
            match commands::add::run_hunks(&pathspecs, &hunks, &lines, unified_context, dry_run) {
                Ok(outcome) => emit_success(outcome, opts, ctx),
                Err(err) => {
                    let code = commands::add::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        } else {
            emit_success(
                commands::add::run(
                    &pathspecs,
                    update,
                    dry_run,
                    intent_to_add,
                    force,
                    executable,
                    non_executable,
                    &mut progress::TerminalProgress::new(opts.mode.is_human()),
                )?,
                opts,
                ctx,
            )
        }
    }
}

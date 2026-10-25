//! `grit stash` — save and restore shelved work.

use std::io::IsTerminal;
use std::io::Write;

use crate::pathspec_args::TrailingPathspecArgs;
use anyhow::{Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::hunk_select::{
    parse_hunk_id_list, report_unstaged_hunk_selection, HunkSelectError,
};
use grit_lib::stash::{StashApplyOptions, StashDiffOptions, StashMode, StashPushOptions};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::commands::diff::{self, DiffOutcome};
use crate::context;
use crate::diff_render::{self, DiffStat};
use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit stash push` (or bare `grit stash`).
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum StashPushOutcome {
    /// Local changes were saved to the stash.
    Saved {
        oid: String,
        message: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dry_run: bool,
        #[serde(default, skip_serializing_if = "StashPushMode::is_default")]
        mode: StashPushMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_hunks: BTreeMap<String, Vec<String>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_lines: BTreeMap<String, Vec<String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// `--dry-run` preview of what would be stashed.
    Preview {
        #[serde(default = "preview_dry_run")]
        dry_run: bool,
        #[serde(default, skip_serializing_if = "StashPushMode::is_default")]
        mode: StashPushMode,
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        untracked: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_hunks: BTreeMap<String, Vec<String>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_lines: BTreeMap<String, Vec<String>>,
    },
    /// Working tree and index already matched `HEAD`; nothing was saved.
    Nothing { stashed: bool },
}

#[derive(Serialize, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StashPushMode {
    #[default]
    All,
    StagedOnly,
    KeepIndex,
}

impl StashPushMode {
    fn is_default(mode: &Self) -> bool {
        matches!(mode, Self::All)
    }
}

impl From<StashMode> for StashPushMode {
    fn from(value: StashMode) -> Self {
        match value {
            StashMode::All => Self::All,
            StashMode::StagedOnly => Self::StagedOnly,
            StashMode::KeepIndex => Self::KeepIndex,
        }
    }
}

fn stash_mode_from_flags(staged: bool, keep_index: bool) -> Result<StashMode> {
    if staged && keep_index {
        anyhow::bail!("cannot combine --staged with --keep-index");
    }
    Ok(if staged {
        StashMode::StagedOnly
    } else if keep_index {
        StashMode::KeepIndex
    } else {
        StashMode::All
    })
}

fn is_false(v: &bool) -> bool {
    !*v
}

const fn preview_dry_run() -> bool {
    true
}

impl HumanRender for StashPushOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Nothing { .. } => outln!(w, "No local changes to save"),
            Self::Saved {
                oid,
                message,
                dry_run,
                ..
            } => {
                let short = oid.get(..7).unwrap_or(oid.as_str());
                let verb = if *dry_run {
                    "Would stash"
                } else {
                    "Saved working directory and index state"
                };
                if message.is_empty() {
                    outln!(w, "{verb} {short}");
                } else {
                    outln!(w, "{verb} {message} {short}");
                }
            }
            Self::Preview {
                paths, untracked, ..
            } => {
                outln!(w, "Would stash {} tracked path(s)", paths.len());
                for path in paths {
                    outln!(w, "  {path}");
                }
                if !untracked.is_empty() {
                    outln!(w, "Would stash {} untracked path(s)", untracked.len());
                    for path in untracked {
                        outln!(w, "  {path}");
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for StashPushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_stash_push_outcome(w, self)?;
        Ok(())
    }
}

/// Result of `grit stash list` (`--json` is the bare array of entries).
#[derive(Serialize, JsonSchema)]
#[serde(transparent)]
pub struct StashListOutcome(pub Vec<StashListEntry>);

#[derive(Serialize, JsonSchema)]
pub struct StashListEntry {
    pub index: usize,
    pub oid: String,
    pub message: String,
}

impl HumanRender for StashListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.0 {
            let short = entry.oid.get(..7).unwrap_or(&entry.oid);
            outln!(w, "stash@{{{}}} {}: {}", entry.index, short, entry.message);
        }
        Ok(())
    }
}

impl MarkdownRender for StashListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.0.is_empty() {
            outln!(w, "- _(no stash entries)_");
            return Ok(());
        }
        for entry in &self.0 {
            let short = entry.oid.get(..7).unwrap_or(&entry.oid);
            outln!(
                w,
                "- **stash@{{{}}}** `{short}` — {}",
                entry.index,
                entry.message
            );
        }
        Ok(())
    }
}

/// Result of `grit stash show`.
#[derive(Serialize, JsonSchema)]
pub struct StashShowOutcome {
    pub index: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_untracked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub patch: Option<DiffOutcome>,
}

impl HumanRender for StashShowOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(patch) = &self.patch {
            if !patch.files.is_empty() {
                outln!(w);
            }
            patch.render_human(w, ctx)?;
            return Ok(());
        }
        if let Some(stat) = &self.stat {
            let color = std::io::stdout().is_terminal()
                && grit_lib::porcelain::terminal::ansi_supported()
                && std::env::var_os("NO_COLOR").is_none();
            diff_render::render_stat_with_leading_blank(w, stat, color, None)?;
        }
        Ok(())
    }
}

impl MarkdownRender for StashShowOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(patch) = &self.patch {
            patch.render_markdown(w, ctx)?;
            return Ok(());
        }
        if let Some(stat) = &self.stat {
            diff_render::render_markdown_stat(w, stat, None)?;
        }
        Ok(())
    }
}

/// Result of `grit stash apply`.
#[derive(Serialize, JsonSchema)]
pub struct StashApplyOutcome {
    pub index: usize,
    pub conflicts: bool,
    pub index_restored: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflict_paths: Vec<String>,
}

impl HumanRender for StashApplyOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.conflicts {
            eprintln!("Stash apply left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
            eprintln!("→ resolve conflicts, then grit add <file> to mark them resolved");
        } else {
            outln!(w, "Applied stash@{{{}}}", self.index);
        }
        Ok(())
    }
}

impl MarkdownRender for StashApplyOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_stash_apply_outcome(w, self)?;
        Ok(())
    }
}

/// Result of `grit stash pop`.
#[derive(Serialize, JsonSchema)]
pub struct StashPopOutcome {
    pub index: usize,
    pub dropped: bool,
    pub conflicts: bool,
    pub index_restored: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflict_paths: Vec<String>,
}

impl StashPopOutcome {
    /// The `grit stash drop` invocation that discards the kept entry once conflicts are resolved.
    pub fn drop_command(&self) -> String {
        match self.index {
            0 => "grit stash drop".to_owned(),
            n => format!("grit stash drop {n}"),
        }
    }
}

impl HumanRender for StashPopOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.conflicts {
            eprintln!("Stash pop left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
            eprintln!("The stash entry was kept.");
            eprintln!(
                "→ resolve conflicts, grit add <file>, then {}",
                self.drop_command()
            );
        } else {
            outln!(
                w,
                "Dropped stash@{{{}}} ({})",
                self.index,
                if self.dropped { "applied" } else { "empty" }
            );
        }
        Ok(())
    }
}

impl MarkdownRender for StashPopOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_stash_pop_outcome(w, self)?;
        Ok(())
    }
}

/// Result of `grit stash drop`.
#[derive(Serialize, JsonSchema)]
pub struct StashDropOutcome {
    pub index: usize,
}

impl HumanRender for StashDropOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "Dropped stash@{{{}}}", self.index);
        Ok(())
    }
}

impl MarkdownRender for StashDropOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_stash_drop_outcome(w, self)?;
        Ok(())
    }
}

/// Exit code for `--hunks` / `--lines` failures on stash push.
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some()
        || err
            .downcast_ref::<LibError>()
            .is_some_and(|e| matches!(e, LibError::HunkSelection(_)))
    {
        2
    } else {
        1
    }
}

/// Arguments for [`run_push`].
pub struct StashPushRunArgs<'a> {
    /// Stash message.
    pub message: Option<String>,
    /// Include untracked paths.
    pub include_untracked: bool,
    /// `--staged` mode.
    pub staged: bool,
    /// `--keep-index` mode.
    pub keep_index: bool,
    /// Partial stash hunk ids.
    pub hunks: &'a [String],
    /// Partial stash line ranges.
    pub lines: &'a [String],
    /// Preview without writing.
    pub dry_run: bool,
    /// Cap dry-run path listing.
    pub limit: Option<usize>,
    /// Trailing pathspec arguments.
    pub pathspec_args: &'a TrailingPathspecArgs,
}

pub fn run_push(args: StashPushRunArgs<'_>) -> Result<StashPushOutcome> {
    let StashPushRunArgs {
        message,
        include_untracked,
        staged,
        keep_index,
        hunks,
        lines,
        dry_run,
        limit,
        pathspec_args,
    } = args;
    let repo = context::discover()?;
    let now = context::wall_clock_now(repo.environment());
    let identity = context::signature(&repo, IdentRole::Committer, now)?;
    let mut selection = if hunks.is_empty() && lines.is_empty() {
        None
    } else {
        let mut sel = parse_hunk_id_list(hunks).map_err(anyhow::Error::new)?;
        sel.ranges = crate::commands::add::parse_line_range_list(lines)?;
        if sel.ids.is_empty() && sel.ranges.is_empty() {
            anyhow::bail!("--hunks or --lines requires at least one hunk id or line range");
        }
        Some(sel)
    };
    if include_untracked && selection.is_some() {
        anyhow::bail!("--include-untracked cannot be used with --hunks or --lines");
    }
    let mode = stash_mode_from_flags(staged, keep_index)?;
    let resolved = pathspec_args.resolve(&repo)?;
    let (pathspecs, _) = resolved.into_pathspec_lists();
    if !pathspecs.is_empty() && selection.is_some() {
        anyhow::bail!("path arguments cannot be used with --hunks or --lines");
    }
    let (stashed_hunks, stashed_lines) = if let Some(ref sel) = selection {
        let report =
            report_unstaged_hunk_selection(&repo, sel, &pathspecs).map_err(anyhow::Error::new)?;
        let hunks = report
            .stashed_hunks
            .into_iter()
            .map(|(path, ids)| {
                (
                    path,
                    ids.into_iter().map(|id| id.as_str().to_string()).collect(),
                )
            })
            .collect();
        (hunks, report.stashed_lines)
    } else {
        (BTreeMap::new(), BTreeMap::new())
    };
    let push_paths = pathspecs.clone();
    let options = StashPushOptions {
        message: message.clone(),
        include_untracked,
        identity,
        selection: selection.take(),
        pathspecs,
        dry_run,
        mode,
    };
    if dry_run {
        let preview = repo.stash().preview(&options).map_err(map_stash_error)?;
        return Ok(match preview {
            Some(p) => {
                let mut paths = p.paths;
                let mut untracked = p.untracked;
                if let Some(n) = limit {
                    paths.truncate(n);
                    untracked.truncate(n.saturating_sub(paths.len()));
                }
                StashPushOutcome::Preview {
                    dry_run: true,
                    mode: mode.into(),
                    paths,
                    untracked,
                    stashed_hunks,
                    stashed_lines,
                }
            }
            None => StashPushOutcome::Nothing { stashed: false },
        });
    }
    let pushed = repo.stash().push(&options).map_err(map_stash_error)?;
    Ok(match pushed {
        Some(result) => StashPushOutcome::Saved {
            oid: result.oid.to_hex(),
            message: result.message,
            dry_run: false,
            mode: mode.into(),
            stashed_hunks,
            stashed_lines,
            paths: push_paths,
        },
        None => StashPushOutcome::Nothing { stashed: false },
    })
}

pub fn run_list() -> Result<StashListOutcome> {
    let repo = context::discover()?;
    let entries = repo.stash().list().map_err(map_stash_error)?;
    Ok(StashListOutcome(
        entries
            .into_iter()
            .map(|e| StashListEntry {
                index: e.index,
                oid: e.oid.to_hex(),
                message: e.message,
            })
            .collect(),
    ))
}

pub fn run_show(
    index: Option<String>,
    patch: bool,
    include_untracked: bool,
) -> Result<StashShowOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    let diff_options = StashDiffOptions { include_untracked };
    let entries = repo
        .stash()
        .diff(n, &diff_options)
        .map_err(map_stash_error)?;
    if patch {
        let patch = diff::outcome_from_tree_entries(&repo, entries)?;
        return Ok(StashShowOutcome {
            index: n,
            include_untracked,
            stat: None,
            patch: Some(patch),
        });
    }
    let full = diff::outcome_from_tree_entries(&repo, entries)?;
    let stat = diff_render::diffstat_from_files(&full.files);
    Ok(StashShowOutcome {
        index: n,
        include_untracked,
        stat: Some(stat),
        patch: None,
    })
}

/// Result of `grit stash branch`.
#[derive(Serialize, JsonSchema)]
pub struct StashBranchOutcome {
    pub branch: String,
    pub base: String,
    pub index: usize,
    pub conflicts: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflict_paths: Vec<String>,
}

impl HumanRender for StashBranchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.conflicts {
            eprintln!("Stash branch left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
        } else {
            outln!(
                w,
                "Created branch '{}' at {} and applied stash@{{{}}}",
                self.branch,
                &self.base[..7.min(self.base.len())],
                self.index
            );
        }
        Ok(())
    }
}

impl MarkdownRender for StashBranchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.conflicts {
            outln!(w, "## Stash branch conflicts\n");
            for path in &self.conflict_paths {
                outln!(w, "- `{path}`");
            }
        } else {
            outln!(
                w,
                "Created branch **{}** at `{}` and applied stash@{{{}}}.",
                self.branch,
                self.base,
                self.index
            );
        }
        Ok(())
    }
}

pub fn run_branch(index: Option<String>, name: &str) -> Result<StashBranchOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash branch needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let outcome = repo.stash().branch(name, n).map_err(map_stash_error)?;
    Ok(StashBranchOutcome {
        branch: outcome.branch,
        base: outcome.base.to_hex(),
        index: outcome.index,
        conflicts: outcome.conflicts,
        conflict_paths: outcome.conflict_paths,
    })
}

/// Result of `grit stash clear`.
#[derive(Serialize, JsonSchema)]
pub struct StashClearOutcome {
    pub cleared: usize,
    pub dry_run: bool,
}

impl HumanRender for StashClearOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            if self.cleared == 0 {
                outln!(w, "No stash entries to clear.");
            } else {
                outln!(
                    w,
                    "{} stash {} would be cleared (use --force)",
                    self.cleared,
                    if self.cleared == 1 {
                        "entry"
                    } else {
                        "entries"
                    }
                );
            }
        } else if self.cleared == 0 {
            outln!(w, "No stash entries to clear.");
        } else {
            outln!(
                w,
                "Cleared {} stash {}",
                self.cleared,
                if self.cleared == 1 {
                    "entry"
                } else {
                    "entries"
                }
            );
        }
        Ok(())
    }
}

impl MarkdownRender for StashClearOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Would clear **{}** stash {}.",
                self.cleared,
                if self.cleared == 1 {
                    "entry"
                } else {
                    "entries"
                }
            );
        } else {
            outln!(
                w,
                "Cleared **{}** stash {}.",
                self.cleared,
                if self.cleared == 1 {
                    "entry"
                } else {
                    "entries"
                }
            );
        }
        Ok(())
    }
}

pub fn run_clear(force: bool, dry_run: bool) -> Result<StashClearOutcome> {
    let repo = context::discover()?;
    let count = repo.stash().list().map_err(map_stash_error)?.len();
    if !force {
        return Ok(StashClearOutcome {
            cleared: count,
            dry_run: true,
        });
    }
    if dry_run {
        return Ok(StashClearOutcome {
            cleared: count,
            dry_run: true,
        });
    }
    let cleared = repo.stash().clear().map_err(map_stash_error)?;
    Ok(StashClearOutcome {
        cleared,
        dry_run: false,
    })
}

pub fn run_apply(index: Option<String>, restore_index: bool) -> Result<StashApplyOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash apply needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let applied = repo
        .stash()
        .apply(n, &StashApplyOptions { restore_index })
        .map_err(map_stash_error)?;
    Ok(StashApplyOutcome {
        index: n,
        conflicts: applied.has_conflicts(),
        index_restored: restore_index,
        conflict_paths: applied.conflicted_paths,
    })
}

pub fn run_pop(index: Option<String>, restore_index: bool) -> Result<StashPopOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash pop needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let popped = repo
        .stash()
        .pop(n, &StashApplyOptions { restore_index })
        .map_err(map_stash_error)?;
    let conflicts = popped.has_conflicts();
    Ok(StashPopOutcome {
        index: n,
        dropped: !conflicts,
        conflicts,
        index_restored: restore_index,
        conflict_paths: popped.conflicted_paths,
    })
}

pub fn run_drop(index: Option<String>) -> Result<StashDropOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    repo.stash().drop(n).map_err(map_stash_error)?;
    Ok(StashDropOutcome { index: n })
}

/// Parse `n` or `stash@{n}` (default `0`).
pub fn parse_stash_index(spec: Option<&str>) -> Result<usize> {
    let raw = spec.unwrap_or("0").trim();
    let digits = if let Some(inner) = raw
        .strip_prefix("stash@{")
        .and_then(|s| s.strip_suffix('}'))
    {
        inner
    } else {
        raw
    };
    digits
        .parse::<usize>()
        .with_context(|| format!("invalid stash index '{raw}'"))
}

fn map_stash_error(err: Error) -> anyhow::Error {
    match err {
        Error::StashNoInitialCommit => {
            anyhow::anyhow!("you do not have the initial commit yet")
        }
        Error::StashNotFound { n: 0 } => anyhow::Error::new(StructuredCliError {
            human: "no stash entries".to_owned(),
            payload: StructuredJsonError {
                hint: Some("save local changes with `grit stash push`".to_owned()),
                ..StructuredJsonError::new("no stash entries", ErrorKind::RevisionNotFound)
            },
        }),
        Error::StashNotFound { n } => anyhow::anyhow!("no stash entry at index {n}"),
        Error::CorruptStash(msg) => anyhow::anyhow!("corrupt stash commit: {msg}"),
        Error::StashWouldOverwriteLocalChanges { paths } => {
            anyhow::anyhow!("local changes would be overwritten by stash:\n{paths}")
        }
        Error::StashCouldNotRestoreUntracked { path } => {
            anyhow::anyhow!("stash untracked restore blocked: '{path}' already exists on disk")
        }
        Error::StashBareRepository => anyhow::anyhow!("cannot stash in a bare repository"),
        Error::StashIndexConflict => {
            anyhow::anyhow!("conflicts in index while applying stash with --index")
        }
        Error::PathspecNoMatch { spec } => {
            anyhow::anyhow!("pathspec '{spec}' did not match any file(s) known to git")
        }
        Error::HunkSelection(err) => anyhow::Error::new(err),
        Error::StashOptionConflict(msg) => anyhow::anyhow!("{msg}"),
        Error::Branch(err) => anyhow::Error::new(err),
        other => anyhow::Error::new(other),
    }
}

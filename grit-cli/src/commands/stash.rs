//! `grit stash` — save and restore shelved work.

use grit_lib::error::DiffError;
use grit_lib::objects::short_hex;
use grit_lib::progress::ProgressSink;
use std::io::IsTerminal;
use std::io::Write;

use crate::pathspec_args::TrailingPathspecArgs;
use anyhow::{Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::diff::DiffOptions;
use grit_lib::diff::HunkId;
use grit_lib::error::Error;
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError};
use grit_lib::stash::{
    StashApplyOptions, StashDiffOptions, StashMode, StashPushOptions, StashReference,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::commands::diff::{self, DiffOutcome};
use crate::context;
use crate::diff_render::{self, DiffStat};
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
                let short = short_hex(oid);
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

/// Result of `grit stash list` (`--json`: `{"action":"list","entries":[…]}`).
#[derive(Serialize, JsonSchema)]
pub struct StashListOutcome {
    action: &'static str,
    pub entries: Vec<StashListEntry>,
}

#[derive(Serialize, JsonSchema)]
pub struct StashListEntry {
    pub index: usize,
    pub oid: String,
    pub message: String,
}

impl StashListOutcome {
    fn new(entries: Vec<StashListEntry>) -> Self {
        Self {
            action: "list",
            entries,
        }
    }
}

impl HumanRender for StashListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            let short = short_hex(&entry.oid);
            outln!(w, "stash@{{{}}} {}: {}", entry.index, short, entry.message);
        }
        Ok(())
    }
}

impl MarkdownRender for StashListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.entries.is_empty() {
            outln!(w, "- _(no stash entries)_");
            return Ok(());
        }
        for entry in &self.entries {
            let short = short_hex(&entry.oid);
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
    pub oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub index: Option<usize>,
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
        } else if let Some(n) = self.index {
            outln!(w, "Applied stash@{{{n}}}");
        } else {
            outln!(w, "Applied stash {}", short_hex(&self.oid));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub oid: Option<String>,
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
    pub oid: String,
}

impl HumanRender for StashDropOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(
            w,
            "Dropped stash@{{{}}} ({})",
            self.index,
            short_hex(&self.oid)
        );
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
            .is_some_and(|e| matches!(e, LibError::Diff(DiffError::HunkSelection(_))))
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
    /// Sink for staging and reset progress.
    pub progress: &'a mut dyn ProgressSink,
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
        progress,
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
    let mode = stash_mode_from_flags(staged, keep_index)?;
    let resolved = pathspec_args.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    if !pathspecs.is_empty() && selection.is_some() {
        anyhow::bail!("path arguments cannot be used with --hunks or --lines");
    }
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
                    stashed_hunks: hunk_id_strings(p.stashed_hunks),
                    stashed_lines: p.stashed_lines,
                }
            }
            None => StashPushOutcome::Nothing { stashed: false },
        });
    }
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let pushed = repo
        .stash()
        .push(
            &options,
            progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_stash_error)?;
    Ok(match pushed {
        Some(result) => StashPushOutcome::Saved {
            oid: result.oid.to_hex(),
            message: result.message,
            dry_run: false,
            mode: mode.into(),
            stashed_hunks: hunk_id_strings(result.stashed_hunks),
            stashed_lines: result.stashed_lines,
            paths: push_paths,
        },
        None => StashPushOutcome::Nothing { stashed: false },
    })
}

fn hunk_id_strings(by_path: BTreeMap<String, Vec<HunkId>>) -> BTreeMap<String, Vec<String>> {
    by_path
        .into_iter()
        .map(|(path, ids)| (path, ids.iter().map(|id| id.as_str().to_owned()).collect()))
        .collect()
}

pub fn run_list() -> Result<StashListOutcome> {
    let repo = context::discover()?;
    let entries = repo.stash().list().map_err(map_stash_error)?;
    Ok(StashListOutcome::new(
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
    let n = resolve_stash_index(&repo, index.as_deref())?;
    let stash_options = StashDiffOptions { include_untracked };
    if patch {
        let entries = repo
            .stash()
            .diff(n, &stash_options)
            .map_err(map_stash_error)?;
        let patch = diff::outcome_from_tree_entries(&repo, entries)?;
        return Ok(StashShowOutcome {
            index: n,
            include_untracked,
            stat: None,
            patch: Some(patch),
        });
    }
    let config = repo.config().snapshot().context("could not load config")?;
    let stat = repo
        .stash()
        .diff_stat(n, &stash_options, DiffOptions::from_config(&config))
        .map_err(map_stash_error)?;
    Ok(StashShowOutcome {
        index: n,
        include_untracked,
        stat: Some(stat.into()),
        patch: None,
    })
}

/// Result of `grit stash branch`.
#[derive(Serialize, JsonSchema)]
pub struct StashBranchOutcome {
    pub branch: String,
    pub base: String,
    pub oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub index: Option<usize>,
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
        } else if let Some(n) = self.index {
            outln!(
                w,
                "Created branch '{}' at {} and applied stash@{{{n}}}",
                self.branch,
                &self.base[..7.min(self.base.len())],
            );
        } else {
            outln!(
                w,
                "Created branch '{}' at {} and applied stash {}",
                self.branch,
                &self.base[..7.min(self.base.len())],
                short_hex(&self.oid)
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
        } else if let Some(n) = self.index {
            outln!(
                w,
                "Created branch **{}** at `{}` and applied stash@{{{n}}}.",
                self.branch,
                self.base,
            );
        } else {
            outln!(
                w,
                "Created branch **{}** at `{}` and applied stash `{}`.",
                self.branch,
                self.base,
                self.oid,
            );
        }
        Ok(())
    }
}

pub fn run_branch(index: Option<String>, name: &str) -> Result<StashBranchOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash branch needs a working tree")?;
    let reference = repo.stash().parse_reference(index.as_deref())?;
    let outcome = repo
        .stash()
        .branch_reference(name, reference)
        .map_err(map_stash_error)?;
    Ok(StashBranchOutcome {
        branch: outcome.branch,
        base: outcome.base.to_hex(),
        oid: outcome.stash_oid.to_hex(),
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

pub fn run_apply(
    index: Option<String>,
    restore_index: bool,
    progress: &mut dyn ProgressSink,
) -> Result<StashApplyOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash apply needs a working tree")?;
    let reference = repo.stash().parse_reference(index.as_deref())?;
    let (stack_index, oid) = stash_reference_parts(&repo, reference)?;
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let applied = repo
        .stash()
        .apply_reference(
            reference,
            &StashApplyOptions {
                restore_index,
                ..Default::default()
            },
            progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_stash_error)?;
    Ok(StashApplyOutcome {
        oid: oid.to_hex(),
        index: stack_index,
        conflicts: applied.has_conflicts(),
        index_restored: restore_index,
        conflict_paths: crate::operation_output::conflict_paths(applied.conflicts),
    })
}

pub fn run_pop(
    index: Option<String>,
    restore_index: bool,
    progress: &mut dyn ProgressSink,
) -> Result<StashPopOutcome> {
    let repo = context::discover()?;
    repo.work_tree()
        .context("grit stash pop needs a working tree")?;
    let reference = repo.stash().parse_reference(index.as_deref())?;
    let (stack_index, oid) = stash_reference_parts(&repo, reference)?;
    let n = stack_index.unwrap_or(0);
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let popped = repo
        .stash()
        .pop_reference(
            reference,
            &StashApplyOptions {
                restore_index,
                ..Default::default()
            },
            progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_stash_error)?;
    let conflicts = popped.has_conflicts();
    Ok(StashPopOutcome {
        index: n,
        oid: Some(oid.to_hex()),
        dropped: !conflicts,
        conflicts,
        index_restored: restore_index,
        conflict_paths: crate::operation_output::conflict_paths(popped.conflicts),
    })
}

pub fn run_drop(index: Option<String>) -> Result<StashDropOutcome> {
    let repo = context::discover()?;
    let reference = repo.stash().parse_reference(index.as_deref())?;
    let (stack_index, _) = stash_reference_parts(&repo, reference)?;
    let n = stack_index.ok_or_else(|| {
        anyhow::anyhow!("stash drop requires a stack index, not a bare commit id")
    })?;
    let oid = repo.stash().drop(n).map_err(map_stash_error)?;
    Ok(StashDropOutcome {
        index: n,
        oid: oid.to_hex(),
    })
}

fn resolve_stash_index(repo: &grit_lib::repo::Repository, spec: Option<&str>) -> Result<usize> {
    let reference = repo
        .stash()
        .parse_reference(spec)
        .map_err(anyhow::Error::new)?;
    match reference {
        StashReference::Index(n) => Ok(n),
        StashReference::Commit(oid) => {
            if let Some(n) = repo.stash().index_for_oid(&oid) {
                Ok(n)
            } else {
                Err(map_stash_error(
                    grit_lib::stash::StashError::NotFound { n: 0 }.into(),
                ))
            }
        }
    }
}

fn stash_reference_parts(
    repo: &grit_lib::repo::Repository,
    reference: StashReference,
) -> Result<(Option<usize>, grit_lib::objects::ObjectId)> {
    match reference {
        StashReference::Index(n) => {
            let oid = repo
                .stash()
                .list()
                .map_err(map_stash_error)?
                .into_iter()
                .find(|e| e.index == n)
                .map(|e| e.oid)
                .ok_or_else(|| {
                    map_stash_error(grit_lib::stash::StashError::NotFound { n }.into())
                })?;
            Ok((Some(n), oid))
        }
        StashReference::Commit(oid) => Ok((repo.stash().index_for_oid(&oid), oid)),
    }
}

fn map_stash_error(err: Error) -> anyhow::Error {
    use grit_lib::stash::{StashError, StashPushValidationError};
    let err = match err {
        Error::Stash(stash) => stash,
        other => return crate::json_error::map_lib_error(other),
    };
    match err {
        StashError::NoInitialCommit => {
            anyhow::anyhow!("you do not have the initial commit yet")
        }
        StashError::NotFound { n } => crate::error_map::stash_not_found_cli(n),
        StashError::CommitNotOnStack { oid } => anyhow::anyhow!(
            "stash commit {} is not on the stash stack; use `grit stash apply {}` instead",
            oid.to_short_hex(),
            oid.to_hex()
        ),
        StashError::CorruptStash { reason, .. } => {
            anyhow::anyhow!("corrupt stash commit: {reason}")
        }
        StashError::WouldOverwriteLocalChanges { paths } => {
            anyhow::anyhow!("local changes would be overwritten by stash:\n{paths}")
        }
        StashError::CouldNotRestoreUntracked { path } => {
            anyhow::anyhow!("stash untracked restore blocked: '{path}' already exists on disk")
        }
        StashError::BareRepository => anyhow::anyhow!("cannot stash in a bare repository"),
        StashError::IndexConflict => {
            anyhow::anyhow!("conflicts in index while applying stash with --index")
        }
        StashError::PushValidation(StashPushValidationError::DryRunUsePreview) => {
            anyhow::anyhow!("dry-run is not supported on push; use preview instead")
        }
        StashError::PushValidation(other) => anyhow::anyhow!("{other}"),
        StashError::Branch(branch_err) => anyhow::Error::new(Error::Branch(branch_err)),
        StashError::HunkSelection(hunk_err) => {
            anyhow::Error::new(Error::Diff(DiffError::HunkSelection(hunk_err)))
        }
        other => anyhow::Error::new(Error::Stash(other)),
    }
}

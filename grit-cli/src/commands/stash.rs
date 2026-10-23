//! `grit stash` — save and restore shelved work.

use std::io::IsTerminal;
use std::io::Write;

use anyhow::{Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError};
use grit_lib::porcelain::stash::{
    apply_stash_entry, drop_stash, list_stashes, pop_stash_entry, push_stash, stash_diff,
    StashCreateOptions,
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
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_hunks: BTreeMap<String, Vec<String>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stashed_lines: BTreeMap<String, Vec<String>>,
    },
    /// Working tree and index already matched `HEAD`; nothing was saved.
    Nothing { stashed: bool },
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl HumanRender for StashPushOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Nothing { .. } => outln!(w, "No local changes to save"),
            Self::Saved { oid, message, .. } => {
                let short = oid.get(..7).unwrap_or(oid.as_str());
                if message.is_empty() {
                    outln!(w, "Saved working directory and index state {short}");
                } else {
                    outln!(
                        w,
                        "Saved working directory and index state {message} {short}"
                    );
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflict_paths: Vec<String>,
}

impl HumanRender for StashPopOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.conflicts {
            eprintln!("Stash pop left conflicts in:");
            for path in &self.conflict_paths {
                eprintln!("  {path}");
            }
            eprintln!("The stash entry was kept.");
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

pub fn run_push(
    message: Option<String>,
    include_untracked: bool,
    hunks: &[String],
    lines: &[String],
    dry_run: bool,
) -> Result<StashPushOutcome> {
    let repo = context::discover()?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::identity(&env, &config, IdentRole::Committer, now)?;
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
    if dry_run && selection.is_none() {
        anyhow::bail!("--dry-run on stash push requires --hunks or --lines");
    }
    if include_untracked && selection.is_some() {
        anyhow::bail!("--include-untracked cannot be used with --hunks or --lines");
    }
    let stashed_hunks = selection
        .as_ref()
        .map(|s| {
            let mut map = BTreeMap::new();
            for id in &s.ids {
                map.entry(String::new())
                    .or_insert_with(Vec::new)
                    .push(id.as_str().to_string());
            }
            map
        })
        .unwrap_or_default();
    let stashed_lines = selection
        .as_ref()
        .map(|s| {
            let mut map = BTreeMap::new();
            for range in &s.ranges {
                let label = if range.start == range.end {
                    format!("{}", range.start)
                } else {
                    format!("{}-{}", range.start, range.end)
                };
                map.entry(range.path.clone())
                    .or_insert_with(Vec::new)
                    .push(label);
            }
            map
        })
        .unwrap_or_default();
    let options = StashCreateOptions {
        message,
        include_untracked,
        identity,
        selection: selection.take(),
        pathspecs: Vec::new(),
        dry_run,
    };
    let pushed = push_stash(&repo, &options).map_err(map_stash_error)?;
    Ok(match pushed {
        Some(oid) => {
            let reflog_message = if dry_run {
                message_from_options(&options.message)
            } else {
                list_stashes(&repo)
                    .map_err(map_stash_error)?
                    .into_iter()
                    .find(|e| e.oid == oid)
                    .map(|e| e.message)
                    .unwrap_or_default()
            };
            StashPushOutcome::Saved {
                oid: oid.to_hex(),
                message: reflog_message,
                dry_run,
                stashed_hunks,
                stashed_lines,
            }
        }
        None => StashPushOutcome::Nothing { stashed: false },
    })
}

fn message_from_options(message: &Option<String>) -> String {
    message.clone().unwrap_or_default()
}

pub fn run_list() -> Result<StashListOutcome> {
    let repo = context::discover()?;
    let entries = list_stashes(&repo).map_err(map_stash_error)?;
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

pub fn run_show(index: Option<String>, patch: bool) -> Result<StashShowOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    if patch {
        let entries = stash_diff(&repo, n).map_err(map_stash_error)?;
        let patch = diff::outcome_from_tree_entries(&repo, entries)?;
        return Ok(StashShowOutcome {
            index: n,
            stat: None,
            patch: Some(patch),
        });
    }
    let entries = stash_diff(&repo, n).map_err(map_stash_error)?;
    let full = diff::outcome_from_tree_entries(&repo, entries)?;
    let stat = diff_render::diffstat_from_files(&full.files);
    Ok(StashShowOutcome {
        index: n,
        stat: Some(stat),
        patch: None,
    })
}

pub fn run_apply(index: Option<String>) -> Result<StashApplyOutcome> {
    let repo = context::discover()?;
    let work_tree = repo
        .work_tree()
        .context("grit stash apply needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let applied = apply_stash_entry(&repo, work_tree, n, false).map_err(map_stash_error)?;
    Ok(StashApplyOutcome {
        index: n,
        conflicts: applied.has_conflicts(),
        conflict_paths: applied.conflicted_paths,
    })
}

pub fn run_pop(index: Option<String>) -> Result<StashPopOutcome> {
    let repo = context::discover()?;
    let work_tree = repo
        .work_tree()
        .context("grit stash pop needs a working tree")?;
    let n = parse_stash_index(index.as_deref())?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::reflog_identity(&env, &config, now);
    let popped = pop_stash_entry(&repo, work_tree, n, false, &identity).map_err(map_stash_error)?;
    let conflicts = popped.has_conflicts();
    Ok(StashPopOutcome {
        index: n,
        dropped: !conflicts,
        conflicts,
        conflict_paths: popped.conflicted_paths,
    })
}

pub fn run_drop(index: Option<String>) -> Result<StashDropOutcome> {
    let repo = context::discover()?;
    let n = parse_stash_index(index.as_deref())?;
    let env = context::environment();
    let config = repo.config().context("could not load config")?;
    let now = context::wall_clock_now(&env);
    let identity = context::reflog_identity(&env, &config, now);
    drop_stash(&repo, n, &identity).map_err(map_stash_error)?;
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
        Error::StashNotFound { n } => anyhow::anyhow!("stash@{{{n}}} is not a valid stash ref"),
        Error::CorruptStash(msg) => anyhow::anyhow!("corrupt stash commit: {msg}"),
        Error::StashWouldOverwriteLocalChanges { paths } => {
            anyhow::anyhow!("local changes would be overwritten by stash:\n{paths}")
        }
        Error::StashCouldNotRestoreUntracked { path } => {
            anyhow::anyhow!("stash untracked restore blocked: '{path}' already exists on disk")
        }
        Error::PathError(msg) if msg.contains("bare") => anyhow::anyhow!("{msg}"),
        Error::HunkSelection(err) => anyhow::Error::new(err),
        other => anyhow::Error::new(other),
    }
}

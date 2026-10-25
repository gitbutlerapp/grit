//! `grit reset` — move the branch tip and optionally the index or worktree.

use anyhow::Result;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::porcelain::reset::{
    reset_to_commit, OrphanedCommitSummary, ResetMode, ResetOptions, ResetOutcome,
    DEFAULT_ORPHAN_LIMIT,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Reset mode in JSON (`soft`, `mixed`, `hard`, or `keep`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetCliMode {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl From<ResetMode> for ResetCliMode {
    fn from(mode: ResetMode) -> Self {
        match mode {
            ResetMode::Soft => Self::Soft,
            ResetMode::Mixed => Self::Mixed,
            ResetMode::Hard => Self::Hard,
            ResetMode::Keep => Self::Keep,
        }
    }
}

/// One commit left unreachable by a reset, or by switching away from a detached `HEAD`.
#[derive(Serialize, JsonSchema)]
pub struct OrphanedCommitJson {
    pub oid: String,
    pub change_id: String,
    pub subject: String,
}

impl From<OrphanedCommitSummary> for OrphanedCommitJson {
    fn from(c: OrphanedCommitSummary) -> Self {
        Self {
            oid: c.oid.to_hex(),
            change_id: c.change_id,
            subject: c.subject,
        }
    }
}

/// CLI view of [`ResetOutcome`].
#[derive(Serialize, JsonSchema)]
pub struct ResetCliOutcome {
    pub mode: ResetCliMode,
    pub target: String,
    pub previous: String,
    pub r#ref: String,
    pub dry_run: bool,
    pub would_overwrite: Vec<String>,
    pub would_block: Vec<String>,
    pub orphaned_commits: Vec<OrphanedCommitJson>,
    pub orphaned_total: usize,
    pub orphaned_truncated: bool,
}

impl From<ResetOutcome> for ResetCliOutcome {
    fn from(value: ResetOutcome) -> Self {
        Self {
            mode: value.mode.into(),
            target: value.target.to_hex(),
            previous: value.previous.to_hex(),
            r#ref: value.ref_updated,
            dry_run: value.dry_run,
            would_overwrite: value.would_overwrite,
            would_block: value.would_block,
            orphaned_commits: value.orphaned_commits.into_iter().map(Into::into).collect(),
            orphaned_total: value.orphaned_total,
            orphaned_truncated: value.orphaned_truncated,
        }
    }
}

impl HumanRender for ResetCliOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Would reset {} to {} ({})",
                self.r#ref,
                &self.target[..7.min(self.target.len())],
                reset_mode_label(self.mode)
            );
            if !self.would_overwrite.is_empty() {
                outln!(w, "Would discard local changes in:");
                for path in &self.would_overwrite {
                    outln!(w, "  {path}");
                }
            }
            if !self.would_block.is_empty() {
                outln!(w, "Would refuse — local changes on:");
                for path in &self.would_block {
                    outln!(w, "  {path}");
                }
            }
        } else {
            outln!(
                w,
                "Reset {} to {} ({})",
                self.r#ref,
                &self.target[..7.min(self.target.len())],
                reset_mode_label(self.mode)
            );
        }
        if self.orphaned_total > 0 {
            outln!(
                w,
                "Warning: {}{} commit(s) would become unreachable",
                self.orphaned_total,
                if self.orphaned_truncated { "+" } else { "" }
            );
        }
        Ok(())
    }
}

impl MarkdownRender for ResetCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Reset");
        outln!(w);
        outln!(w, "- **ref:** `{}`", self.r#ref);
        outln!(w, "- **mode:** `{}`", reset_mode_label(self.mode));
        outln!(w, "- **target:** `{}`", self.target);
        outln!(w, "- **previous:** `{}`", self.previous);
        outln!(w, "- **dry_run:** `{}`", self.dry_run);
        if self.orphaned_total > 0 {
            outln!(
                w,
                "- **orphaned_total:** `{}` (truncated: `{}`)",
                self.orphaned_total,
                self.orphaned_truncated
            );
        }
        Ok(())
    }
}

fn reset_mode_label(mode: ResetCliMode) -> &'static str {
    match mode {
        ResetCliMode::Soft => "soft",
        ResetCliMode::Mixed => "mixed",
        ResetCliMode::Hard => "hard",
        ResetCliMode::Keep => "keep",
    }
}

/// `--mixed` is the default mode; the flag is accepted so Git habits keep working.
pub fn run(
    rev: Option<String>,
    soft: bool,
    _mixed: bool,
    hard: bool,
    keep: bool,
    dry_run: bool,
    orphan_limit: Option<usize>,
) -> Result<ResetCliOutcome> {
    let mode = if soft {
        ResetMode::Soft
    } else if hard {
        ResetMode::Hard
    } else if keep {
        ResetMode::Keep
    } else {
        ResetMode::Mixed
    };

    let repo = context::discover()?;
    let spec = rev.unwrap_or_else(|| "HEAD".to_owned());
    let target = resolve_revision_as_commit_without_index_dwim(&repo, &spec)
        .map_err(|err| map_reset_committish_error(err, &spec))?;

    let env = repo.environment();
    let now = context::wall_clock_now(env);
    let identity = context::signature(&repo, IdentRole::Committer, now)?;

    let message = format!("reset: moving to {spec}");
    let mut opts = ResetOptions::new(target, mode, identity, message);
    opts.dry_run = dry_run;
    opts.orphan_limit = orphan_limit.unwrap_or(DEFAULT_ORPHAN_LIMIT);

    let outcome = reset_to_commit(
        &repo,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .map_err(anyhow::Error::new)?;

    Ok(outcome.into())
}

fn map_reset_committish_error(err: Error, spec: &str) -> anyhow::Error {
    match err {
        Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        Error::ObjectNotFound(msg) => {
            anyhow::anyhow!("invalid revision '{spec}': {msg}")
        }
        other => anyhow::Error::new(other),
    }
}

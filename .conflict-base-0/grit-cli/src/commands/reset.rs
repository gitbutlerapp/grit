//! `grit reset` — move the branch tip and optionally the index or worktree.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions, ResetOutcome};
use grit_lib::revwalk::rev_parse::resolve_revision;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Reset mode in JSON (`soft`, `mixed`, or `hard`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetCliMode {
    Soft,
    Mixed,
    Hard,
}

impl From<ResetMode> for ResetCliMode {
    fn from(mode: ResetMode) -> Self {
        match mode {
            ResetMode::Soft => Self::Soft,
            ResetMode::Mixed => Self::Mixed,
            ResetMode::Hard => Self::Hard,
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
}

impl From<ResetOutcome> for ResetCliOutcome {
    fn from(value: ResetOutcome) -> Self {
        Self {
            mode: value.mode.into(),
            target: value.target.to_hex(),
            previous: value.previous.to_hex(),
            r#ref: value.ref_updated,
        }
    }
}

impl HumanRender for ResetCliOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(
            w,
            "Reset {} to {} ({})",
            self.r#ref,
            &self.target[..7.min(self.target.len())],
            reset_mode_label(self.mode)
        );
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
        Ok(())
    }
}

fn reset_mode_label(mode: ResetCliMode) -> &'static str {
    match mode {
        ResetCliMode::Soft => "soft",
        ResetCliMode::Mixed => "mixed",
        ResetCliMode::Hard => "hard",
    }
}

/// `--mixed` is the default mode; the flag is accepted so Git habits keep working.
pub fn run(rev: Option<String>, soft: bool, _mixed: bool, hard: bool) -> Result<ResetCliOutcome> {
    let mode = if soft {
        ResetMode::Soft
    } else if hard {
        ResetMode::Hard
    } else {
        ResetMode::Mixed
    };

    let repo = context::discover()?;
    let spec = rev.unwrap_or_else(|| "HEAD".to_owned());
    let target =
        resolve_revision(&repo, &spec).with_context(|| format!("invalid revision '{spec}'"))?;

    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let now = context::wall_clock_now(env);
    let identity = context::reflog_identity(env, &config, now);

    let message = format!("reset: moving to {spec}");
    let outcome = reset_to_commit(
        &repo,
        &ResetOptions {
            target,
            mode,
            identity,
            message,
        },
    )
    .map_err(anyhow::Error::new)?;

    Ok(outcome.into())
}

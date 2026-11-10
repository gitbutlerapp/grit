//! `grit switch` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::porcelain::reset::OrphanedCommitSummary;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
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

/// Result of `grit switch`.
#[derive(Serialize, JsonSchema)]
pub struct SwitchOutcome {
    /// Branch `HEAD` is on after the switch, or JSON `null` when detached.
    pub branch: Option<String>,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<usize>,
    /// Hooks that ran after the switch (`post-checkout` when installed).
    pub hooks_run: Vec<String>,
    /// Newest-first commits only the previous detached `HEAD` reached (capped at 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub stranded_commits: Vec<OrphanedCommitJson>,
    /// Total commits left behind; may exceed `stranded_commits` when truncated.
    #[serde(default, skip_serializing_if = "is_zero")]
    #[schemars(default)]
    /// Advisory count; without a commit-graph, severe clock skew can make it inaccurate.
    pub stranded_total: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub stranded_truncated: bool,
    /// Index paths that collided during checkout on a case-insensitive filesystem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub path_collisions: Vec<String>,
}

impl SwitchOutcome {
    /// Warn on stderr that commits were (or would be) left behind, with a command to keep them.
    fn warn_stranded(&self) {
        let Some(newest) = self.stranded_commits.first() else {
            return;
        };
        let count = self.stranded_total;
        let (noun, them) = if count == 1 {
            ("commit", "it")
        } else {
            ("commits", "them")
        };
        let would = if self.dry_run { " would be" } else { "" };
        eprintln!("Warning: {count} {noun}{would} left behind, not on any branch:");
        for commit in &self.stranded_commits {
            eprintln!("  {} {}", short_hex(&commit.oid), commit.subject);
        }
        if self.stranded_truncated {
            eprintln!("  … and {} more", count - self.stranded_commits.len());
        }
        eprintln!(
            "Keep {them} on a branch with: grit branch <name> {}",
            newest.oid
        );
    }

    /// The branch name, or the short detached commit, for human and Markdown output.
    pub fn target_label(&self) -> &str {
        self.branch.as_deref().unwrap_or_else(|| {
            self.commit
                .as_deref()
                .map(|c| &c[..7.min(c.len())])
                .unwrap_or("HEAD")
        })
    }

    fn render_switch_line(&self, w: &mut dyn Write) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Would switch to {} ({} path(s) would change)",
                self.target_label(),
                self.changed_paths.unwrap_or(0)
            );
            return Ok(());
        }
        let Some(branch) = self.branch.as_deref() else {
            outln!(w, "HEAD detached at {}", self.target_label());
            return render_hooks_run(w, &self.hooks_run);
        };
        if self.created {
            outln!(w, "Created and switched to branch {branch}");
            if let Some(up) = self.upstream.as_deref() {
                outln!(w, "  tracking {up}");
            }
        } else {
            outln!(w, "Switched to branch {branch}");
        }
        render_hooks_run(w, &self.hooks_run)
    }
}

impl HumanRender for SwitchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        self.render_switch_line(w)?;
        if !self.stranded_commits.is_empty() {
            crate::stdio::io_result(w.flush())
                .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
            self.warn_stranded();
        }
        Ok(())
    }
}

impl MarkdownRender for SwitchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_switch_outcome(w, self)
    }
}

fn render_hooks_run(w: &mut dyn Write, hooks_run: &[String]) -> Result<()> {
    if !hooks_run.is_empty() {
        outln!(w, "hooks: {}", hooks_run.join(", "));
    }
    Ok(())
}

/// Markdown body for a [`SwitchOutcome`].
pub fn print_switch_outcome(w: &mut dyn Write, outcome: &SwitchOutcome) -> Result<()> {
    outln!(w, "## Switch");
    outln!(w);
    if outcome.dry_run {
        outln!(
            w,
            "- Would switch to **`{}`** ({} path(s) would change)",
            outcome.target_label(),
            outcome.changed_paths.unwrap_or(0)
        );
        return print_switch_stranded(w, outcome);
    }
    match outcome.branch.as_deref() {
        None => outln!(w, "- HEAD detached at `{}`", outcome.target_label()),
        Some(branch) if outcome.created => {
            if let Some(up) = outcome.upstream.as_deref() {
                outln!(
                    w,
                    "- Created and switched to **`{branch}`** (tracking `{up}`)"
                );
            } else {
                outln!(w, "- Created and switched to **`{branch}`**");
            }
        }
        Some(branch) => outln!(w, "- Switched to **`{branch}`**"),
    }
    if !outcome.hooks_run.is_empty() {
        outln!(w, "- Hooks: `{}`", outcome.hooks_run.join("`, `"));
    }
    if !outcome.path_collisions.is_empty() {
        outln!(
            w,
            "- **Path collisions** (case or Unicode normalization; one on-disk file per group):"
        );
        for path in &outcome.path_collisions {
            outln!(w, "  - `{path}`");
        }
    }
    print_switch_stranded(w, outcome)
}

fn print_switch_stranded(w: &mut dyn Write, outcome: &SwitchOutcome) -> Result<()> {
    let Some(newest) = outcome.stranded_commits.first() else {
        return Ok(());
    };
    let count = outcome.stranded_total;
    let (noun, them) = if count == 1 {
        ("commit", "it")
    } else {
        ("commits", "them")
    };
    let would = if outcome.dry_run { " would be" } else { "" };
    outln!(w);
    outln!(
        w,
        "> **Warning:** {count} {noun}{would} left behind, not on any branch:"
    );
    outln!(w);
    for commit in &outcome.stranded_commits {
        outln!(w, "- `{}` {}", short_hex(&commit.oid), commit.subject);
    }
    if outcome.stranded_truncated {
        outln!(w, "- … and {} more", count - outcome.stranded_commits.len());
    }
    outln!(w);
    outln!(w, "Keep {them} with `grit branch <name> {}`.", newest.oid);
    Ok(())
}

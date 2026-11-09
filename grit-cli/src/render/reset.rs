//! `grit reset` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::switch::OrphanedCommitJson;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// Reset mode in JSON (`soft`, `mixed`, `hard`, or `keep`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetCliMode {
    Soft,
    Mixed,
    Hard,
    Keep,
}

/// CLI view of a reset operation.
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

fn reset_mode_label(mode: ResetCliMode) -> &'static str {
    match mode {
        ResetCliMode::Soft => "soft",
        ResetCliMode::Mixed => "mixed",
        ResetCliMode::Hard => "hard",
        ResetCliMode::Keep => "keep",
    }
}

impl ResetCliOutcome {
    fn previous_short(&self) -> &str {
        &self.previous[..7.min(self.previous.len())]
    }

    fn orphan_count_label(&self) -> String {
        if self.orphaned_truncated {
            format!("{}+", self.orphaned_total)
        } else {
            self.orphaned_total.to_string()
        }
    }

    fn recovery_command(&self, previous_oid: &str) -> String {
        let mode_flag = match self.mode {
            ResetCliMode::Mixed => "",
            ResetCliMode::Soft => " --soft",
            ResetCliMode::Hard => " --hard",
            ResetCliMode::Keep => " --keep",
        };
        format!("grit reset{mode_flag} {previous_oid}")
    }

    fn render_orphan_warning(&self, w: &mut dyn Write) -> Result<()> {
        if self.orphaned_total == 0 {
            return Ok(());
        }
        let count = self.orphan_count_label();
        let previous = self.previous_short();
        let plural = self.orphaned_total != 1;
        let (noun, recover) = if plural {
            ("commits", "them")
        } else {
            ("commit", "it")
        };
        let recovery_cmd = self.recovery_command(previous);
        if self.dry_run {
            outln!(
                w,
                "Warning: {count} {noun} would leave {} (was {previous}). Recover {recover} with `{recovery_cmd}`.",
                self.r#ref,
            );
        } else {
            let verb = if plural { "are" } else { "is" };
            outln!(
                w,
                "Warning: {count} {noun} {verb} no longer on {} (was {previous}). Recover {recover} with `{recovery_cmd}` (recorded in ORIG_HEAD and, when reflogs are on, the reflog).",
                self.r#ref,
            );
        }
        Ok(())
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
        self.render_orphan_warning(w)
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
            let recover = if self.orphaned_total == 1 {
                "it"
            } else {
                "them"
            };
            outln!(
                w,
                "- **recovery:** `{}` restores {recover} (also recorded in ORIG_HEAD; reflog when enabled)",
                self.recovery_command(&self.previous)
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HumanRender;

    fn sample_outcome(
        mode: ResetCliMode,
        dry_run: bool,
        orphaned_total: usize,
        orphaned_truncated: bool,
    ) -> ResetCliOutcome {
        ResetCliOutcome {
            mode,
            target: "062ca6e74f5603f4d09063c0f46cd07d8c96e019".into(),
            previous: "83352a61f6715ec3f0a297c6d6150725154fb833".into(),
            r#ref: "refs/heads/main".into(),
            dry_run,
            would_overwrite: vec![],
            would_block: vec![],
            orphaned_commits: vec![],
            orphaned_total,
            orphaned_truncated,
        }
    }

    fn render_markdown_text(outcome: &ResetCliOutcome) -> String {
        let ctx = RenderContext::human_for_tests();
        let mut buf = Vec::new();
        outcome
            .render_markdown(&mut buf, &ctx)
            .expect("render markdown");
        String::from_utf8(buf).expect("utf8")
    }

    fn render_human_text(outcome: &ResetCliOutcome) -> String {
        let ctx = RenderContext::human_for_tests();
        let mut buf = Vec::new();
        outcome.render_human(&mut buf, &ctx).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn orphan_warning_past_tense_after_reset() {
        let text = render_human_text(&sample_outcome(ResetCliMode::Mixed, false, 1, false));
        assert!(text.contains("83352a6"), "{text}");
        assert!(text.contains("is no longer on refs/heads/main"));
        assert!(text.contains("Recover it with `grit reset 83352a6`"));
        assert!(!text.contains("would"));
    }

    #[test]
    fn orphan_warning_includes_mode_flag_per_reset_mode() {
        let previous = "83352a6";
        let mixed = render_human_text(&sample_outcome(ResetCliMode::Mixed, false, 1, false));
        assert!(
            mixed.contains(&format!("`grit reset {previous}`")),
            "{mixed}"
        );
        let soft = render_human_text(&sample_outcome(ResetCliMode::Soft, false, 1, false));
        assert!(
            soft.contains(&format!("`grit reset --soft {previous}`")),
            "{soft}"
        );
        let hard = render_human_text(&sample_outcome(ResetCliMode::Hard, false, 1, false));
        assert!(
            hard.contains(&format!("`grit reset --hard {previous}`")),
            "{hard}"
        );
        let keep = render_human_text(&sample_outcome(ResetCliMode::Keep, false, 1, false));
        assert!(
            keep.contains(&format!("`grit reset --keep {previous}`")),
            "{keep}"
        );
    }

    #[test]
    fn orphan_warning_future_tense_on_dry_run() {
        let text = render_human_text(&sample_outcome(ResetCliMode::Mixed, true, 1, false));
        assert!(text.contains("83352a6"), "{text}");
        assert!(text.contains("would leave refs/heads/main"));
        assert!(!text.contains("is no longer"));
    }

    #[test]
    fn orphan_warning_hard_dry_run_includes_hard_in_recovery() {
        let text = render_human_text(&sample_outcome(ResetCliMode::Hard, true, 1, false));
        assert!(
            text.contains("Recover it with `grit reset --hard 83352a6`"),
            "{text}"
        );
    }

    #[test]
    fn orphan_warning_markdown_recovery_uses_full_oid_and_mode() {
        let md = render_markdown_text(&sample_outcome(ResetCliMode::Soft, false, 1, false));
        assert!(
            md.contains(
                "**recovery:** `grit reset --soft 83352a61f6715ec3f0a297c6d6150725154fb833`"
            ),
            "{md}"
        );
    }

    #[test]
    fn orphan_warning_plural_and_truncated() {
        let text = render_human_text(&sample_outcome(ResetCliMode::Mixed, false, 25, true));
        assert!(text.contains("25+ commits are no longer"), "{text}");
        let dry = render_human_text(&sample_outcome(ResetCliMode::Mixed, true, 25, true));
        assert!(dry.contains("25+ commits would leave"), "{dry}");
    }
}

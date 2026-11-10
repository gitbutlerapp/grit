//! `grit stash` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;

use crate::commands::stash::{
    StashApplyOutcome, StashBranchOutcome, StashClearOutcome, StashDropOutcome, StashListOutcome,
    StashPopOutcome, StashPushOutcome, StashShowOutcome,
};
use crate::diff_render;
use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::path_display_for_discovered_repo;

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
            let path_opts = path_display_for_discovered_repo();
            diff_render::render_stat_with_leading_blank(w, stat, ctx.color, path_opts.as_ref())?;
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
            let path_opts = path_display_for_discovered_repo();
            diff_render::render_markdown_stat(w, stat, path_opts.as_ref())?;
        }
        Ok(())
    }
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

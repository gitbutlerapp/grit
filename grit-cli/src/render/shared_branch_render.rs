//! Human and Markdown rendering for [`super::shared_branch`] report types.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;

use crate::output::outln;
use crate::render::shared_branch::SharedHead;
use crate::render::shared_branch::{DivergenceJson, HeadRelation, IntegratedJson};

impl HeadRelation {
    pub(crate) fn words(self) -> &'static str {
        match self {
            Self::Same => "same as yours",
            Self::Ahead => "has commits you don't",
            Self::Behind => "behind yours",
            Self::Diverged => "diverged from yours",
            Self::Unknown => "not compared",
        }
    }
}

impl SharedHead {
    #[must_use]
    pub(crate) fn needs_decision(&self) -> bool {
        self.divergence.as_ref().is_some_and(|d| d.needs_decision)
    }
}

impl DivergenceJson {
    fn short_change(change: &str) -> String {
        match change.strip_prefix("patch:") {
            Some(patch) => format!("patch {}", short_hex(patch)),
            None => change.chars().take(8).collect(),
        }
    }

    pub(crate) fn render_human(&self, w: &mut dyn Write, indent: &str) -> Result<()> {
        let mark = if self.needs_decision { "!" } else { "·" };
        outln!(w, "{indent}{mark} {} ({})", self.summary, self.scenario);
        for c in &self.changes {
            outln!(
                w,
                "{indent}  {:<8}  you {:<9} them {:<9} {}",
                Self::short_change(&c.change),
                c.you,
                c.them,
                c.subject
            );
        }
        if !self.options.is_empty() {
            outln!(w, "{indent}  options");
            let width = self
                .options
                .iter()
                .map(|o| o.label.len())
                .max()
                .unwrap_or(0);
            for (i, o) in self.options.iter().enumerate() {
                let loses = o
                    .loses
                    .as_deref()
                    .map(|l| format!("  (leaves out {l})"))
                    .unwrap_or_default();
                outln!(
                    w,
                    "{indent}    {}  {:<width$}  {}{loses}",
                    i + 1,
                    o.label,
                    o.command
                );
            }
        }
        Ok(())
    }

    pub(crate) fn render_markdown(&self, w: &mut dyn Write, indent: &str) -> Result<()> {
        let decide = if self.needs_decision {
            "needs a decision"
        } else {
            "can be put together without losing anything"
        };
        outln!(
            w,
            "{indent}- **Scenario** `{}` ({decide}): {}",
            self.scenario,
            self.summary
        );
        for c in &self.changes {
            outln!(
                w,
                "{indent}  - `{}` {}: you {}, them {}",
                Self::short_change(&c.change),
                c.subject,
                c.you,
                c.them
            );
        }
        if !self.conflicts.is_empty() {
            let paths: Vec<String> = self.conflicts.iter().map(|p| format!("`{p}`")).collect();
            outln!(w, "{indent}  - Conflicts in {}", paths.join(", "));
        }
        if !self.options.is_empty() {
            outln!(w);
            outln!(
                w,
                "{indent}| Option | What it does | Leaves out | Command |"
            );
            outln!(w, "{indent}| --- | --- | --- | --- |");
            for (i, o) in self.options.iter().enumerate() {
                let recommended = if i == 0 { " (recommended)" } else { "" };
                outln!(
                    w,
                    "{indent}| `{}`{recommended} | {} | {} | `{}` |",
                    o.id,
                    o.label,
                    o.loses.as_deref().unwrap_or("nothing"),
                    o.command
                );
            }
        }
        Ok(())
    }
}

impl IntegratedJson {
    pub(crate) fn render_human(&self, w: &mut dyn Write, branch: &str) -> Result<()> {
        let pull = self
            .pull
            .map(|n| format!(" (pull request {n})"))
            .unwrap_or_default();
        outln!(
            w,
            "! {branch} was shared, and {} integrated it into {} {}{pull}; it's a branch nobody shares now",
            self.by,
            self.into,
            self.target_branch
        );
        outln!(
            w,
            "  {} of its commits {} integrated already; {} {} new",
            self.integrated,
            if self.integrated == 1 { "was" } else { "were" },
            self.new,
            if self.new == 1 { "is" } else { "are" }
        );
        let width = self
            .options
            .iter()
            .map(|o| o.label.len())
            .max()
            .unwrap_or(0);
        for (i, o) in self.options.iter().enumerate() {
            outln!(w, "    {}  {:<width$}  {}", i + 1, o.label, o.command);
        }
        Ok(())
    }

    pub(crate) fn render_markdown(&self, w: &mut dyn Write, branch: &str) -> Result<()> {
        outln!(w, "**Scenario:** `{}`. {}.", self.scenario, self.summary);
        if let Some(url) = &self.url {
            outln!(w, "- Pull request: {url}");
        }
        outln!(
            w,
            "- Integrated into `{into}` branch `{target}` by **{by}** at {at}",
            into = self.into,
            target = self.target_branch,
            by = self.by,
            at = self.at
        );
        outln!(
            w,
            "- {integrated} commits were already integrated; {new} are new",
            integrated = self.integrated,
            new = self.new
        );
        if !self.options.is_empty() {
            outln!(w);
            outln!(w, "| Option | Command |");
            outln!(w, "| --- | --- |");
            for o in &self.options {
                outln!(w, "| {} | `{}` |", o.label, o.command);
            }
        }
        let _ = branch;
        Ok(())
    }
}

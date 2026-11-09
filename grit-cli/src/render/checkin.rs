//! JSON outcome types and rendering for `grit checkin` and `grit integrate`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::remote::snapshot_checkout::CheckinRule;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::sync::SyncTarget;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// One branch of `grit checkin`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckinBranchJson {
    /// The branch.
    pub name: String,
    /// `created`, `updated`, `unchanged` or `refused`; `unchanged` when the
    /// server's branch was at the graft already, or ahead of it.
    pub status: String,
    /// The commit on the server's history the branch became.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grafted: Option<String>,
    /// Where the server's branch was before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Commits replayed onto the server's history.
    pub commits: usize,
    /// For an `updated` branch, what allowed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<CheckinRuleJson>,
    /// Why it was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// What kind of refusal it was: `invalid_name`, `target_branch`,
    /// `case_conflict`, `invalid_tip`, `not_pushed`, `not_on_snapshot`,
    /// `conflict`, `merge_conflict` or `diverged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why the server let a check-in move a branch it already had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckinRuleJson {
    /// The server's branch is in the new commit's history.
    FastForward,
    /// This checkout set the branch last, so it replaces its own work.
    OwnLastSet,
    /// A rebase that keeps every commit the server's branch had, by change id.
    KeepsEveryCommit,
    /// `--force` replaced the branch.
    Forced,
}

impl From<CheckinRule> for CheckinRuleJson {
    fn from(rule: CheckinRule) -> Self {
        match rule {
            CheckinRule::FastForward => Self::FastForward,
            CheckinRule::OwnLastSet => Self::OwnLastSet,
            CheckinRule::KeepsEveryCommit => Self::KeepsEveryCommit,
            CheckinRule::Forced => Self::Forced,
        }
    }
}

impl CheckinRuleJson {
    /// The rule in words.
    fn words(self) -> &'static str {
        match self {
            Self::FastForward => "fast-forward",
            Self::OwnLastSet => "replaces what this checkout checked in before",
            Self::KeepsEveryCommit => "rebased, keeps every commit the server's branch had",
            Self::Forced => "forced",
        }
    }
}

/// Result of `grit checkin`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckinOutcome {
    /// The server repository, with any credentials removed.
    pub url: String,
    /// Commits made with plain Git that were given a change id first (their
    /// branches were rewritten with it).
    pub gave_change_ids: usize,
    /// One entry per branch; empty when no branch had commits to check in.
    pub branches: Vec<CheckinBranchJson>,
    /// Whether any branch was refused (the exit status is then `1`).
    pub refused: bool,
}

/// What `grit checkin` says when no branch has work on the snapshot.
const NOTHING_TO_CHECK_IN: &str =
    "nothing to check in: no branch has commits on top of the snapshot";

fn gave_change_ids_phrase(n: usize) -> String {
    format!(
        "{n} commit{} made with git a change id",
        if n == 1 { "" } else { "s" }
    )
}

impl HumanRender for CheckinOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.gave_change_ids > 0 {
            outln!(w, "· gave {}", gave_change_ids_phrase(self.gave_change_ids));
        }
        if self.branches.is_empty() {
            outln!(w, "· {NOTHING_TO_CHECK_IN}");
        }
        for b in &self.branches {
            let at = b.grafted.as_deref().map(short_hex).unwrap_or_default();
            let commits = if b.commits == 1 {
                "1 commit".to_owned()
            } else {
                format!("{} commits", b.commits)
            };
            let rule = b
                .rule
                .map(|r| format!("; {}", r.words()))
                .unwrap_or_default();
            match b.status.as_str() {
                "created" => outln!(w, "✓ {} created on the server at {at} ({commits})", b.name),
                "updated" => outln!(
                    w,
                    "✓ {} updated on the server {} → {at} ({commits}{rule})",
                    b.name,
                    b.previous.as_deref().map(short_hex).unwrap_or_default()
                ),
                "unchanged" => match b.previous.as_deref() {
                    Some(previous) if b.grafted.as_deref() != Some(previous) => outln!(
                        w,
                        "· {} has nothing new: the server's is ahead, at {} (`grit pull {}` brings it)",
                        b.name,
                        short_hex(previous),
                        b.name
                    ),
                    _ => outln!(w, "· {} already at {at} on the server", b.name),
                },
                _ => outln!(
                    w,
                    "✗ {}: {}",
                    b.name,
                    b.message.as_deref().unwrap_or("refused")
                ),
            }
        }
        Ok(())
    }
}

impl MarkdownRender for CheckinOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Check-in to `{}`", self.url);
        outln!(w);
        if self.gave_change_ids > 0 {
            outln!(
                w,
                "- Change ids: gave {}",
                gave_change_ids_phrase(self.gave_change_ids)
            );
        }
        if self.branches.is_empty() {
            outln!(w, "- {NOTHING_TO_CHECK_IN}");
            return Ok(());
        }
        if self.gave_change_ids > 0 {
            outln!(w);
        }
        outln!(w, "| Branch | Status | Server commit | Commits | Rule |");
        outln!(w, "| --- | --- | --- | ---: | --- |");
        for b in &self.branches {
            outln!(
                w,
                "| `{}` | {} | {} | {} | {} |",
                b.name,
                b.status,
                b.grafted
                    .as_deref()
                    .map(|g| format!("`{g}`"))
                    .unwrap_or_default(),
                b.commits,
                b.rule.map(CheckinRuleJson::words).unwrap_or_default()
            );
        }
        for b in self.branches.iter().filter(|b| b.message.is_some()) {
            outln!(w);
            outln!(
                w,
                "- `{}`: {}",
                b.name,
                b.message.as_deref().unwrap_or_default()
            );
        }
        Ok(())
    }
}

/// Result of `grit integrate`.
#[derive(Serialize, JsonSchema)]
pub struct IntegrateOutcome {
    /// The branch merged.
    pub name: String,
    /// The server's target branch.
    pub target: String,
    /// `merged` or `already_integrated`.
    pub status: String,
    /// Where the target was.
    pub previous: String,
    /// Where it is now.
    pub head: String,
    /// The branch's commits the merge brought in.
    pub commits: usize,
    /// The check-in done first, when the branch is here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkin: Option<CheckinOutcome>,
    /// The refresh done after (as `grit pull` reports it); omitted when it
    /// failed (see `refresh_error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<crate::commands::pull::PullOutcome>,
    /// The new work branch you are on, when the branch you were on was the
    /// one merged: that branch is gone here (it stays on the server).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    /// The branch this checkout follows, when the server merged into another
    /// one (its `HEAD` names a different branch now): the merge is not in this
    /// checkout's snapshots, so the branch keeps its commits here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    /// Why refreshing this checkout after the merge (or moving to a new work
    /// branch) failed. The merge stands on the server; the exit status is 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    /// In a clone of a hosted project: the project merged into,
    /// `<owner>/<project>` (an organization's, for a copy in your space).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The pull request the merge went through, for a copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull: Option<u64>,
    /// The pull request's web page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// In a clone: the target branch here, brought up to the project's after
    /// the merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<SyncTarget>,
    /// For a shared branch (`grit share`): the other members. Integrating
    /// it ended the share and deleted the branch from their copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub unshared: Vec<String>,
}

impl IntegrateOutcome {
    /// Whether the checkout needs attention after the merge: its refresh
    /// failed, or stopped on a conflict or a refusal (the exit status is 1).
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.refresh_error.is_some()
            || self.refresh.as_ref().is_some_and(|r| {
                crate::exit_status::ExitStatus::from_pull(r)
                    != crate::exit_status::ExitStatus::Success
            })
    }
}

impl HumanRender for IntegrateOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(c) = &self.checkin {
            c.render_human(w, ctx)?;
        }
        let into = match (&self.project, self.pull) {
            (Some(project), Some(pull)) => {
                format!("{project} {} (pull request {pull})", self.target)
            }
            (Some(project), None) => format!("{project} {}", self.target),
            _ => self.target.clone(),
        };
        match self.status.as_str() {
            "merged" => outln!(
                w,
                "✓ Merged {} into {into} on the server: {} → {} ({} commit{})",
                self.name,
                short_hex(&self.previous),
                short_hex(&self.head),
                self.commits,
                if self.commits == 1 { "" } else { "s" }
            ),
            "started" => outln!(
                w,
                "✓ {into} starts at {}: {}",
                self.name,
                short_hex(&self.head)
            ),
            _ => outln!(w, "· {into} already has every commit of {}", self.name),
        }
        if let Some(url) = &self.url {
            outln!(w, "  {url}");
        }
        if let Some(local) = &self.local {
            let tip = local.tip.as_deref().map(short_hex).unwrap_or_default();
            outln!(w, "· {} here: {} ({tip})", local.name, local.action.word());
        }
        if !self.unshared.is_empty() {
            outln!(
                w,
                "· {} was shared with {}: the share is over, and the branch is gone from their copies",
                self.name,
                self.unshared.join(", ")
            );
        }
        if let Some(follows) = &self.follows {
            outln!(
                w,
                "· This checkout follows {follows}, not {}, so {} keeps its commits here.",
                self.target,
                self.name
            );
        }
        if let Some(r) = &self.refresh {
            r.render_human(w, ctx)?;
        }
        if let Some(to) = &self.moved_to {
            outln!(
                w,
                "{} is in {} now, so it's gone here. You're on {to}, a new branch.",
                self.name,
                self.target
            );
        }
        if let Some(e) = &self.refresh_error {
            if self.refresh.is_some() {
                outln!(w, "✗ {e}");
            } else {
                outln!(w, "✗ Refreshing this checkout failed: {e}");
            }
            outln!(
                w,
                "  The merge stands on the server; fix that, then run `grit integrate {}` again to finish.",
                self.name
            );
        }
        Ok(())
    }
}

impl MarkdownRender for IntegrateOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(c) = &self.checkin {
            c.render_markdown(w, ctx)?;
            outln!(w);
        }
        outln!(w, "## Integrate `{}` into `{}`", self.name, self.target);
        outln!(w);
        outln!(w, "- **status**: {}", self.status);
        outln!(w, "- **previous**: `{}`", self.previous);
        outln!(w, "- **head**: `{}`", self.head);
        outln!(w, "- **commits**: {}", self.commits);
        if let Some(project) = &self.project {
            outln!(w, "- **project**: `{project}`");
        }
        if let Some(pull) = self.pull {
            outln!(w, "- **pull request**: {pull}");
        }
        if let Some(url) = &self.url {
            outln!(w, "- **url**: {url}");
        }
        if !self.unshared.is_empty() {
            outln!(
                w,
                "- **unshared**: {} (the share is over; the branch is gone from their copies)",
                self.unshared.join(", ")
            );
        }
        if let Some(follows) = &self.follows {
            outln!(
                w,
                "- **follows**: `{follows}` (the merge went into `{}`, so `{}` keeps its commits here)",
                self.target,
                self.name
            );
        }
        if let Some(to) = &self.moved_to {
            outln!(
                w,
                "- **moved to**: `{to}`, a new branch (`{}` is in `{}` now, so it's gone here)",
                self.name,
                self.target
            );
        }
        if let Some(e) = &self.refresh_error {
            outln!(
                w,
                "- **refresh failed**: {e} → the merge stands on the server; fix that, then `grit integrate {}` again to finish",
                self.name
            );
        }
        if let Some(r) = &self.refresh {
            outln!(w);
            r.render_markdown(w, ctx)?;
        }
        Ok(())
    }
}

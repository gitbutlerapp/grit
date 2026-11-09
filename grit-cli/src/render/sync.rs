//! JSON outcome types and rendering for `grit sync`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::share::{DivergenceJson, HeadRelation, IntegratedJson};
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// Result of `grit sync`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SyncOutcome {
    /// Your space's project, `<you>/<project>`.
    pub space: String,
    /// The project it follows, `<owner>/<project>`, for a copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The remote branches are pushed to and pulled from.
    pub remote: String,
    /// Whether this run set the clone up to push to your space.
    pub configured: bool,
    /// What happened to the target branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SyncTarget>,
    /// What happened to each active branch.
    pub branches: Vec<SyncBranch>,
}

impl SyncOutcome {
    /// Whether a shared branch's heads diverged in a way someone has to
    /// decide about.
    #[must_use]
    pub fn needs_decision(&self) -> bool {
        self.branches
            .iter()
            .any(|b| b.integrated.is_some() || b.shared.iter().any(SharedHead::needs_decision))
    }

    /// Whether a branch could not be synced (diverged or refused).
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.branches
            .iter()
            .any(|b| matches!(b.action, SyncAction::Diverged | SyncAction::Refused))
            || self
                .target
                .as_ref()
                .is_some_and(|t| t.action == TargetAction::Diverged)
    }
}

/// What `grit sync` did with the target branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SyncTarget {
    /// The branch, such as `main`.
    pub name: String,
    pub action: TargetAction,
    /// Its tip here afterwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
}

/// What happened to the target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetAction {
    /// It was already the project's.
    UpToDate,
    /// Moved forward to the project's.
    FastForwarded,
    /// Made here at the project's.
    Created,
    /// It has commits the project doesn't; left alone.
    Ahead,
    /// It and the project's both have commits the other lacks; left alone.
    Diverged,
}

impl TargetAction {
    /// The action in words for human and Markdown output.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::UpToDate => "up to date",
            Self::FastForwarded => "fast-forwarded",
            Self::Created => "created",
            Self::Ahead => "ahead of upstream",
            Self::Diverged => "diverged from upstream",
        }
    }
}

/// One branch `grit sync` looked at.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SyncBranch {
    pub name: String,
    pub action: SyncAction,
    /// Its tip afterwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
    /// Why it was refused, or what to do about a divergence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// For a shared branch: the other members' heads, compared with yours
    /// afterwards.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub shared: Vec<SharedHead>,
    /// For a branch this sync made again after a share of it was
    /// integrated (scenario `pushed_to_integrated`): what happened, and the
    /// options. Nobody shares it now.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub integrated: Option<IntegratedJson>,
}

/// Another member's head of a shared branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SharedHead {
    /// Their user name.
    pub user: String,
    /// Their head's tip.
    pub tip: String,
    /// How it compares with your branch here.
    pub relation: HeadRelation,
    /// For a head that diverged from yours: what happened, and whether
    /// someone has to decide what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub divergence: Option<DivergenceJson>,
}

impl SharedHead {
    /// Whether someone has to decide what to do about this head.
    pub(crate) fn needs_decision(&self) -> bool {
        self.divergence.as_ref().is_some_and(|d| d.needs_decision)
    }
}

/// What happened to one branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncAction {
    /// Pushed to your space.
    Pushed,
    /// Fast-forwarded here to your space's.
    Pulled,
    /// Made here from your space's.
    Created,
    /// The same here and in your space.
    UpToDate,
    /// Here and in your space, each with commits the other lacks.
    Diverged,
    /// The server refused the push.
    Refused,
}

impl SyncAction {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Pushed => "pushed",
            Self::Pulled => "pulled",
            Self::Created => "created",
            Self::UpToDate => "up to date",
            Self::Diverged => "diverged",
            Self::Refused => "refused",
        }
    }
}

impl HumanRender for SyncOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match &self.upstream {
            Some(upstream) => outln!(w, "Syncing {} (a copy of {upstream})", self.space),
            None => outln!(w, "Syncing {}", self.space),
        }
        if self.configured {
            outln!(
                w,
                "· Pushes from this clone now go to {} (remote `{}`)",
                self.space,
                self.remote
            );
        }
        if let Some(t) = &self.target {
            let tip = t.tip.as_deref().map(short_hex).unwrap_or_default();
            match t.action {
                TargetAction::UpToDate => outln!(w, "· {} is up to date ({tip})", t.name),
                TargetAction::FastForwarded => outln!(w, "✓ {} fast-forwarded to {tip}", t.name),
                TargetAction::Created => outln!(w, "✓ {} created at {tip}", t.name),
                TargetAction::Ahead => outln!(
                    w,
                    "· {} has commits upstream doesn't; it changes only by integrating",
                    t.name
                ),
                TargetAction::Diverged => outln!(
                    w,
                    "✗ {} and upstream's have both moved; it changes only by integrating, so reset it to upstream's",
                    t.name
                ),
            }
        }
        if self.branches.is_empty() {
            outln!(w, "· No branches to sync");
        }
        for b in &self.branches {
            let mark = match b.action {
                SyncAction::Diverged | SyncAction::Refused => "✗",
                SyncAction::UpToDate => "·",
                _ => "✓",
            };
            let tip = b.tip.as_deref().map(short_hex).unwrap_or_default();
            outln!(w, "{mark} {} {} {tip}", b.name, b.action.word());
            if let Some(m) = &b.message {
                outln!(w, "  {m}");
            }
            if let Some(i) = &b.integrated {
                i.render_human(w, &b.name)?;
            }
            for h in &b.shared {
                outln!(
                    w,
                    "  {:<10} {}  {}",
                    h.user,
                    short_hex(&h.tip),
                    h.relation.words()
                );
                if let Some(d) = &h.divergence {
                    d.render_human(w, "    ")?;
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for SyncOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Sync `{}`", self.space);
        outln!(w);
        if let Some(upstream) = &self.upstream {
            outln!(w, "- **upstream**: `{upstream}`");
        }
        outln!(w, "- **remote**: `{}`", self.remote);
        if let Some(t) = &self.target {
            outln!(w, "- **{}**: {}", t.name, t.action.word());
        }
        for b in &self.branches {
            match &b.message {
                Some(m) => outln!(w, "- `{}`: {} — {m}", b.name, b.action.word()),
                None => outln!(w, "- `{}`: {}", b.name, b.action.word()),
            }
            if let Some(i) = &b.integrated {
                outln!(w);
                i.render_markdown(w, &b.name)?;
                outln!(w);
            }
            for h in &b.shared {
                outln!(
                    w,
                    "  - `{}`'s head `{}`: {}",
                    h.user,
                    short_hex(&h.tip),
                    h.relation.words()
                );
                if let Some(d) = &h.divergence {
                    d.render_markdown(w, "    ")?;
                }
            }
        }
        Ok(())
    }
}

//! JSON outcome types and rendering for `grit share`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

#[allow(unused_imports)]
pub use crate::render::shared_branch::IntegratedJson;
pub use crate::render::shared_branch::{DivergenceJson, HeadRelation, ResolveOptionJson};

/// A member of a share, as `grit share` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShareMemberJson {
    /// Their user name.
    pub user: String,
    /// Whether they shared it first.
    pub sharer: bool,
    /// Whether this is you.
    pub you: bool,
    /// The tip of their head on the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Their copy, `<user>/<project>`, once they have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// How their head compares with your branch here (for other members).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<HeadRelation>,
    /// When their head diverged from yours: what happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub divergence: Option<DivergenceJson>,
    /// For a member with a snapshot share (`--no-history`): the paths they
    /// see, empty for the whole tree. Omitted for a full share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub snapshot: Option<Vec<String>>,
}

/// A share link, as `grit share` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShareLinkJson {
    /// Its id, for `grit share revoke`.
    pub id: String,
    /// The URL to send: only when the link was just made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Uses left; omitted when it has no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uses_left: Option<u32>,
    /// When it stops working (RFC 3339).
    pub expires_at: String,
    /// For a snapshot share link: the paths it gives, empty for the whole
    /// tree. Omitted for a full share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub snapshot: Option<Vec<String>>,
}

/// A shared branch, as `grit share` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShareBranchJson {
    /// The branch.
    pub branch: String,
    /// The project it was shared from, `<sharer>/<project>`.
    pub origin: String,
    /// Who shared it.
    pub sharer: String,
    /// `active`, `integrated` or `ended`.
    pub state: String,
    /// `full` or `snapshot`.
    pub mode: String,
    /// The read-only branch members' copies follow for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Everyone on it, the sharer first.
    pub members: Vec<ShareMemberJson>,
    /// Email addresses invited who haven't accepted yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub invited: Vec<String>,
    /// Open share links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub links: Vec<ShareLinkJson>,
    /// Its web page.
    pub url: String,
}

/// Result of `grit share add`, `list`, `remove`, `leave` and `revoke`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ShareOutcome {
    /// `add`, `list`, `remove`, `leave` or `revoke`.
    pub action: String,
    /// Your project the shares are in, `<you>/<project>`.
    pub project: String,
    /// The shares, as they are now.
    pub shares: Vec<ShareBranchJson>,
    /// The link `grit share add --link` made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<ShareLinkJson>,
    /// People added, who were notified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub added: Vec<String>,
    /// People removed (or you, when leaving).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub removed: Vec<String>,
    /// Whether the branch was pushed to your space first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub pushed: bool,
}

/// Result of `grit share accept`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ShareAcceptOutcome {
    /// `accepted` or `already_member`.
    pub status: String,
    /// The shared branch.
    pub branch: String,
    /// Who shared it.
    pub sharer: String,
    /// The project it was shared from.
    pub origin: String,
    /// Your copy, `<you>/<project>`.
    pub project: String,
    /// Your copy's clone URL.
    pub url: String,
    /// The clone it was fetched into, or cloned as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether this made a new clone.
    pub cloned: bool,
}
/// Result of `grit share resolve`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ShareResolveOutcome {
    /// The shared branch.
    pub branch: String,
    /// The member whose head yours was put together with.
    pub with: String,
    /// What the two heads added up to, before.
    pub scenario: String,
    /// What happened, in a sentence.
    pub summary: String,
    /// The option applied, or omitted when none was (a decision is needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// Your head before.
    pub previous: String,
    /// Your head afterwards, when it moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Whether your head was pushed.
    pub pushed: bool,
    /// What to run next, when the option is done by hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The ways out, when a decision is needed and none was chosen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub options: Vec<ResolveOptionJson>,
    /// For `rename`: the branch's new name, here and in your space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub renamed: Option<String>,
    /// For `reshare`: who the branch is shared with again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub shared_with: Vec<String>,
}

impl ShareResolveOutcome {
    /// Whether a decision is still needed: nothing was chosen.
    pub fn needs_decision(&self) -> bool {
        self.option.is_none() && !self.options.is_empty()
    }
}
impl HumanRender for ShareResolveOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "{} ({})", self.summary, self.scenario);
        match (&self.option, &self.head) {
            (None, _) => {
                outln!(w, "! Choose what to do:");
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
                    outln!(w, "  {}  {:<width$}  {}{loses}", i + 1, o.label, o.command);
                }
            }
            (Some(option), Some(head)) => {
                let moved = if *head == self.previous {
                    format!("{} stays at {}", self.branch, short_hex(head))
                } else {
                    format!(
                        "{} {} → {}",
                        self.branch,
                        short_hex(&self.previous),
                        short_hex(head)
                    )
                };
                outln!(w, "✓ {option} with {}'s head: {moved}", self.with);
                if self.pushed {
                    outln!(w, "✓ pushed your head");
                }
            }
            (Some(_), None) if self.renamed.is_some() => {
                let to = self.renamed.as_deref().unwrap_or_default();
                outln!(
                    w,
                    "✓ renamed {} to {to}, here and in your space",
                    self.branch
                );
            }
            (Some(_), None) if !self.shared_with.is_empty() => {
                outln!(
                    w,
                    "✓ shared {} again with {}",
                    self.branch,
                    self.shared_with.join(", ")
                );
            }
            (Some(option), None) => {
                outln!(
                    w,
                    "· {option}: {}",
                    self.next.as_deref().unwrap_or_default()
                );
            }
        }
        Ok(())
    }
}

impl MarkdownRender for ShareResolveOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Resolve `{}` with `{}`", self.branch, self.with);
        outln!(w);
        outln!(w, "**Scenario:** `{}`. {}", self.scenario, self.summary);
        outln!(w);
        match (&self.option, &self.head) {
            (None, _) => {
                outln!(w, "| Option | What it does | Leaves out | Command |");
                outln!(w, "| --- | --- | --- | --- |");
                for (i, o) in self.options.iter().enumerate() {
                    let recommended = if i == 0 { " (recommended)" } else { "" };
                    outln!(
                        w,
                        "| `{}`{recommended} | {} | {} | `{}` |",
                        o.id,
                        o.label,
                        o.loses.as_deref().unwrap_or("nothing"),
                        o.command
                    );
                }
                outln!(w);
                outln!(w, "Exit status 3: a decision is needed.");
            }
            (Some(option), Some(head)) => {
                outln!(
                    w,
                    "Applied `{option}`: `{}` is now `{}`{}.",
                    self.branch,
                    short_hex(head),
                    if self.pushed { ", pushed" } else { "" }
                );
            }
            (Some(_), None) if self.renamed.is_some() => {
                outln!(
                    w,
                    "Renamed `{}` to `{}`, here and in your space.",
                    self.branch,
                    self.renamed.as_deref().unwrap_or_default()
                );
            }
            (Some(_), None) if !self.shared_with.is_empty() => {
                outln!(
                    w,
                    "Shared `{}` again with {}.",
                    self.branch,
                    self.shared_with.join(", ")
                );
            }
            (Some(option), None) => {
                outln!(
                    w,
                    "`{option}` is done by hand: {}",
                    self.next.as_deref().unwrap_or_default()
                );
            }
        }
        Ok(())
    }
}
fn snapshot_words(paths: &[String]) -> String {
    if paths.is_empty() {
        "snapshot, no history".to_owned()
    } else {
        format!("snapshot of {}", paths.join(", "))
    }
}

fn render_branch_human(w: &mut dyn Write, s: &ShareBranchJson) -> Result<()> {
    let others: Vec<&str> = s
        .members
        .iter()
        .filter(|m| !m.you)
        .map(|m| m.user.as_str())
        .collect();
    if others.is_empty() {
        outln!(w, "{}, shared by {} (nobody else yet)", s.branch, s.sharer);
    } else {
        outln!(w, "{}, shared with {}", s.branch, others.join(", "));
    }
    let width = s
        .members
        .iter()
        .map(|m| m.user.len())
        .max()
        .unwrap_or(3)
        .max(3);
    for m in &s.members {
        let name = if m.you { "you" } else { m.user.as_str() };
        let head = m.head.as_deref().map(short_hex).unwrap_or("-");
        let note = match (&m.head, m.relation) {
            (None, _) => "hasn't fetched it yet".to_owned(),
            (_, Some(r)) => {
                let hint = match r {
                    HeadRelation::Ahead => "   grit sync",
                    _ => "",
                };
                format!("{}{hint}", r.words())
            }
            _ => String::new(),
        };
        let note = match &m.snapshot {
            Some(paths) if note.is_empty() => format!("({})", snapshot_words(paths)),
            Some(paths) => format!("{note}  ({})", snapshot_words(paths)),
            None => note,
        };
        let line = format!("  {name:<width$}  {head:<7}  {note}");
        outln!(w, "{}", line.trim_end());
        if let Some(d) = &m.divergence {
            d.render_human(w, "    ")?;
        }
    }
    for email in &s.invited {
        outln!(w, "  {email}  invited by email; hasn't accepted yet");
    }
    for l in &s.links {
        let uses = match l.uses_left {
            Some(n) => format!("{n} use{} left", if n == 1 { "" } else { "s" }),
            None => "no use limit".to_owned(),
        };
        let view = l
            .snapshot
            .as_deref()
            .map(|p| format!(", {}", snapshot_words(p)))
            .unwrap_or_default();
        outln!(w, "  link {}  {uses}, until {}{view}", l.id, l.expires_at);
    }
    Ok(())
}

impl HumanRender for ShareOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.pushed {
            for s in &self.shares {
                outln!(w, "· pushed {} to {} first", s.branch, self.project);
            }
        }
        match self.action.as_str() {
            "add" => {
                for user in &self.added {
                    if user.contains('@') {
                        outln!(w, "✓ invited {user} by email, to sign up and join");
                    } else {
                        outln!(w, "✓ shared with {user}, who is emailed");
                    }
                }
            }
            "remove" | "leave" => {
                for user in &self.removed {
                    if user.contains('@') {
                        outln!(w, "✓ withdrew the invitation to {user}");
                    } else {
                        outln!(w, "✓ removed {user}");
                    }
                }
                if self.shares.is_empty() && self.action == "remove" {
                    outln!(
                        w,
                        "✓ the share has ended; every head stays a branch in its owner's space"
                    );
                }
                if self.action == "leave" {
                    outln!(w, "✓ left; your head stays a branch in your space");
                }
            }
            "revoke" => outln!(w, "✓ link revoked"),
            _ => {}
        }
        if let Some(link) = &self.link {
            let uses = match link.uses_left {
                Some(n) => format!("{n} use{}", if n == 1 { "" } else { "s" }),
                None => "no use limit".to_owned(),
            };
            let view = link
                .snapshot
                .as_deref()
                .map(|p| format!(", {}", snapshot_words(p)))
                .unwrap_or_default();
            outln!(w, "✓ share link ({uses}, until {}{view}):", link.expires_at);
            outln!(w, "  {}", link.url.as_deref().unwrap_or_default());
        }
        if self.shares.is_empty() && self.action == "list" {
            outln!(
                w,
                "· No shared branches in {}; share one with `grit share add <branch> <user>`",
                self.project
            );
        }
        for s in &self.shares {
            render_branch_human(w, s)?;
        }
        Ok(())
    }
}

impl MarkdownRender for ShareOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Shared branches in `{}`", self.project);
        outln!(w);
        let notes = usize::from(self.pushed)
            + self.added.len()
            + self.removed.len()
            + usize::from(self.link.is_some());
        if self.pushed {
            outln!(w, "- Pushed the branch to your space first.");
        }
        for user in &self.added {
            if user.contains('@') {
                outln!(w, "- Invited `{user}` by email, to sign up and join.");
            } else {
                outln!(w, "- Shared with `{user}`, who is emailed.");
            }
        }
        for user in &self.removed {
            if user.contains('@') {
                outln!(w, "- Withdrew the invitation to `{user}`.");
            } else {
                outln!(w, "- Removed `{user}`.");
            }
        }
        if let Some(link) = &self.link {
            let uses = match link.uses_left {
                Some(n) => n.to_string(),
                None => "unlimited".to_owned(),
            };
            let view = link
                .snapshot
                .as_deref()
                .map(|p| format!(", {}", snapshot_words(p)))
                .unwrap_or_default();
            outln!(
                w,
                "- Share link (uses: {uses}, until {}{view}): {}",
                link.expires_at,
                link.url.as_deref().unwrap_or_default()
            );
        }
        if self.shares.is_empty() {
            if notes > 0 {
                outln!(w);
            }
            outln!(w, "No shared branches.");
        }
        for (i, s) in self.shares.iter().enumerate() {
            if i > 0 || notes > 0 {
                outln!(w);
            }
            outln!(w, "### `{}`", s.branch);
            outln!(w);
            outln!(
                w,
                "Shared by `{}` from `{}`; state {}, base `{}`.",
                s.sharer,
                s.origin,
                s.state,
                s.base_branch.as_deref().unwrap_or("-")
            );
            outln!(w);
            outln!(w, "| Member | Head | Compared with yours |");
            outln!(w, "| --- | --- | --- |");
            for m in &s.members {
                let name = if m.you {
                    format!("`{}` (you)", m.user)
                } else {
                    format!("`{}`", m.user)
                };
                let head = m
                    .head
                    .as_deref()
                    .map(|h| format!("`{}`", short_hex(h)))
                    .unwrap_or_else(|| "not fetched yet".to_owned());
                let rel = m.relation.map(HeadRelation::words).unwrap_or("-");
                let name = match &m.snapshot {
                    Some(paths) => format!("{name} ({})", snapshot_words(paths)),
                    None => name,
                };
                outln!(w, "| {name} | {head} | {rel} |");
            }
            for email in &s.invited {
                outln!(w, "| {email} | invited by email, hasn't accepted | - |");
            }
            for m in &s.members {
                if let Some(d) = &m.divergence {
                    outln!(w);
                    outln!(w, "`{}`'s head diverged from yours:", m.user);
                    outln!(w);
                    d.render_markdown(w, "")?;
                }
            }
            if !s.links.is_empty() {
                outln!(w);
                outln!(w, "| Link | Uses left | Expires | Gives |");
                outln!(w, "| --- | --- | --- | --- |");
                for l in &s.links {
                    let uses = l
                        .uses_left
                        .map_or_else(|| "unlimited".to_owned(), |n| n.to_string());
                    let gives = l
                        .snapshot
                        .as_deref()
                        .map_or_else(|| "the branch with history".to_owned(), snapshot_words);
                    outln!(w, "| `{}` | {uses} | {} | {gives} |", l.id, l.expires_at);
                }
            }
        }
        Ok(())
    }
}

impl HumanRender for ShareAcceptOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.status.as_str() {
            "already_member" => outln!(w, "· You're already on {}", self.branch),
            _ => outln!(w, "✓ Joined {}, shared by {}", self.branch, self.sharer),
        }
        outln!(w, "  your copy  {}", self.project);
        if let Some(path) = &self.path {
            if self.cloned {
                outln!(w, "  cloned to  {path}");
            } else {
                outln!(w, "  fetched    {path}");
            }
        }
        outln!(
            w,
            "Switch to it with `grit switch {}`; push your head with `grit push`, and `grit sync` brings the others' in.",
            self.branch
        );
        Ok(())
    }
}

impl MarkdownRender for ShareAcceptOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Share `{}`", self.branch);
        outln!(w);
        outln!(w, "- **status**: {}", self.status);
        outln!(w, "- **shared by**: `{}`", self.sharer);
        outln!(w, "- **your copy**: `{}`", self.project);
        if let Some(path) = &self.path {
            let verb = if self.cloned {
                "cloned to"
            } else {
                "fetched into"
            };
            outln!(w, "- **{verb}**: `{path}`");
        }
        outln!(w);
        outln!(
            w,
            "Next: `grit switch {}`, commit, `grit push`; `grit sync` brings in the other members' heads.",
            self.branch
        );
        Ok(())
    }
}

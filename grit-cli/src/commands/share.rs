//! `grit share` — work on a branch together with other users of a grit
//! hosting server such as grithub.
//!
//! A shared branch is in every member's space, and each member has their
//! own head of it: only you move yours, so nobody's push can overwrite
//! anyone else's work, and pushing your head after an amend or rebase needs
//! no `--force`. The other members' heads are fetched beside yours as
//! `<remote>/shared/<branch>/<user>`.
//!
//! - `grit share add <branch> [<user>...] [--link]` shares a branch of your
//!   space (pushing it first when it isn't there yet), adds people, or makes
//!   a share link anyone signed in can accept.
//! - `grit share list [<branch>]` shows your shares: who is on each, where
//!   their heads are, and how each compares with your branch here.
//! - `grit share accept <link>` accepts a link and fetches the branch (or
//!   clones your copy when you aren't in a clone of the project).
//! - `grit share remove <branch> [<user>...]` removes people, or ends the
//!   share; `grit share leave <branch>` leaves it; `grit share revoke
//!   <branch> <link-id>` revokes a link.

use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::refs::store::RefName;
use grit_lib::remote::grit_server_auth::{ServerAuthError, ServerUrl};
use grit_lib::remote::grit_server_share::{
    self as api, LinkOptions, LinkUses, ShareAcceptRequest, ShareAcceptStatus, ShareMode,
    ShareReply, ShareRequest, ShareState, SharedBranch,
};
use grit_lib::remote::HttpClientFactory as _;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::sync::{configure_shared, ensure_space, fetch, push, tips};
use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

/// How another member's head compares with your branch here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadRelation {
    /// The same commit as yours.
    Same,
    /// It has commits yours doesn't, and yours is in its history: `grit
    /// sync` fast-forwards to it.
    Ahead,
    /// Yours has commits it doesn't, and it is in yours.
    Behind,
    /// Each has commits the other lacks.
    Diverged,
    /// You have no branch here, or its commits aren't fetched.
    Unknown,
}

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

/// One commit of a divergence: what each side did to it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DivergenceChangeJson {
    /// Its change id, or `patch:<patch id>` for a commit without one.
    pub change: String,
    /// Its subject line.
    pub subject: String,
    /// What you did to it: `unchanged`, `moved`, `reworded`, `amended`,
    /// `dropped`, `squashed`, `split`, `new`, `split_piece` or `absent`.
    pub you: String,
    /// What they did to it, in the same words.
    pub them: String,
    /// The paths its versions change.
    pub paths: Vec<String>,
}

/// One way out of a divergence.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResolveOptionJson {
    /// Its stable id: `combine`, `keep_mine`, `keep_theirs`, `keep_both`,
    /// `keep_amended`, `drop`, `my_order`, `their_order`, `my_split`,
    /// `their_split`, `restore` or `resolve`.
    pub id: String,
    /// What it does.
    pub label: String,
    /// What it leaves out (kept in the share's log), or `null` when it keeps
    /// every change: such an option is safe to choose without asking.
    pub loses: Option<String>,
    /// The command that applies it.
    pub command: String,
}

/// How another member's head diverged from yours
/// ([`grit_lib::divergence`]).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DivergenceJson {
    /// The scenario's stable code, such as `amended_both_conflicting`.
    pub scenario: String,
    /// What happened, in a sentence.
    pub summary: String,
    /// Whether someone has to decide what to do. When `false`, the heads can
    /// be put together without losing anything.
    pub needs_decision: bool,
    /// The last head you both had, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The commits either side changed or added, oldest first.
    pub changes: Vec<DivergenceChangeJson>,
    /// Paths where the two heads' changes conflict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflicts: Vec<String>,
    /// Other scenarios that also apply, most pressing first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub also: Vec<String>,
    /// The ways out, the recommended one first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub options: Vec<ResolveOptionJson>,
}

impl DivergenceJson {
    pub(crate) fn from_divergence(
        d: &grit_lib::divergence::Divergence,
        them: &str,
        branch: &str,
        remote: &str,
    ) -> Self {
        let options = d
            .options(them)
            .into_iter()
            .map(|o| ResolveOptionJson {
                id: o.choice.id().to_owned(),
                label: o.label,
                loses: o.loses,
                command: match o.choice {
                    grit_lib::divergence::Choice::Resolve => {
                        format!("grit rebase {remote}/shared/{branch}/{them}")
                    }
                    choice => format!("grit share resolve {branch} {} --with {them}", choice.id()),
                },
            })
            .collect();
        Self {
            options,
            scenario: d.scenario.code().to_owned(),
            summary: d.summary(them),
            needs_decision: d.scenario.needs_decision(),
            base: d.base.map(|b| b.to_hex()),
            changes: d
                .changes
                .iter()
                .map(|c| DivergenceChangeJson {
                    change: c.key.clone(),
                    subject: c.subject.clone(),
                    you: c.ours.word().to_owned(),
                    them: c.theirs.word().to_owned(),
                    paths: c.paths.clone(),
                })
                .collect(),
            conflicts: d.conflicts.iter().map(|p| p.as_str().to_owned()).collect(),
            also: d
                .scenarios
                .iter()
                .skip(1)
                .map(|s| s.code().to_owned())
                .collect(),
        }
    }

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

/// Explain how `user`'s head `theirs` of shared `branch` (fetched from
/// `remote`) diverged from yours, `ours`. `None` when it can't be worked
/// out (objects missing, say).
pub(crate) fn explain(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<DivergenceJson> {
    let (floor, base) = comparison(repo, remote, branch, user, ours, theirs)?;
    let d = repo
        .revwalk()
        .classify_divergence(&floor, base, ours, theirs)
        .ok()?;
    Some(DivergenceJson::from_divergence(&d, user, branch, remote))
}

/// What comparing two heads needs: the floor (the base branch's commits)
/// and the last head both had.
pub(crate) fn comparison(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<(Vec<ObjectId>, Option<ObjectId>)> {
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .ok()?;
    let mut floor = Vec::new();
    if let Some(base) =
        crate::context::config_string(&config, &format!("branch.{branch}.sharedBase"))
    {
        for name in [
            format!("refs/remotes/origin/{base}"),
            format!("refs/remotes/{remote}/{base}"),
            format!("refs/heads/{base}"),
        ] {
            if let Ok(oid) = grit_lib::refs::resolve_ref(repo.git_dir(), &name) {
                if !floor.contains(&oid) {
                    floor.push(oid);
                }
            }
        }
    }
    let seen = |name: &str| {
        RefName::try_from(name)
            .map(|name| repo.revwalk().heads_seen(&name))
            .unwrap_or_default()
    };
    let ours_seen = seen(&format!("refs/heads/{branch}"));
    let theirs_seen = seen(&format!("refs/remotes/{remote}/shared/{branch}/{user}"));
    let base = repo
        .revwalk()
        .divergence_base(ours, theirs, &ours_seen, &theirs_seen)
        .ok()?;
    Some((floor, base))
}

/// Put your branch and `user`'s head back together as `choice` says,
/// without moving anything: the new head, or why not.
pub(crate) fn put_together(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
    choice: grit_lib::divergence::Choice,
) -> Result<std::result::Result<ObjectId, String>> {
    let (floor, base) =
        comparison(repo, remote, branch, user, ours, theirs).context("comparing the two heads")?;
    let made = repo
        .revwalk()
        .combine_divergence(&floor, base, ours, theirs, choice)
        .context("putting the two heads together")?;
    Ok(match made.head {
        Some(head) => Ok(head),
        None => Err(format!(
            "{} conflicts in {}",
            made.stopped_at
                .map(|s| format!("\"{s}\""))
                .unwrap_or_else(|| "a commit".to_owned()),
            made.conflicts
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

/// Move your `branch` to `head`. When it is checked out, the working tree
/// follows, keeping local changes (`grit reset --keep`, which refuses
/// rather than overwrite them).
pub(crate) fn move_branch(repo: &Repository, branch: &str, head: ObjectId) -> Result<()> {
    let checked_out = matches!(
        grit_lib::state::resolve_head_for_repository(repo),
        Ok(grit_lib::state::HeadState::Branch { short_name, .. }) if short_name == branch
    );
    if checked_out {
        crate::commands::reset::run(Some(head.to_hex()), false, false, false, true, false, None)
            .with_context(|| format!("moving {branch} (it is checked out)"))?;
    } else {
        grit_lib::refs::write_ref(repo.git_dir(), format!("refs/heads/{branch}"), &head)
            .with_context(|| format!("moving {branch}"))?;
    }
    Ok(())
}

/// When `user`'s head and yours can be put together without losing
/// anything, and that changes yours, do it: the new head of your branch.
pub(crate) fn auto_combine(
    repo: &Repository,
    remote: &str,
    branch: &str,
    user: &str,
    ours: ObjectId,
    theirs: ObjectId,
) -> Option<ObjectId> {
    let (floor, base) = comparison(repo, remote, branch, user, ours, theirs)?;
    let d = repo
        .revwalk()
        .classify_divergence(&floor, base, ours, theirs)
        .ok()?;
    if !d.scenario.diverged() || d.scenario.needs_decision() {
        return None;
    }
    let head = put_together(
        repo,
        remote,
        branch,
        user,
        ours,
        theirs,
        grit_lib::divergence::Choice::Combine,
    )
    .ok()?
    .ok()?;
    // Yours already has everything theirs does: nothing to do here; they
    // take yours when they sync.
    if head == ours {
        return None;
    }
    move_branch(repo, branch, head).ok()?;
    Some(head)
}

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

fn state_word(state: ShareState) -> &'static str {
    match state {
        ShareState::Active => "active",
        ShareState::Integrated => "integrated",
        ShareState::Ended => "ended",
    }
}

fn mode_word(mode: ShareMode) -> &'static str {
    match mode {
        ShareMode::Full => "full",
        ShareMode::Snapshot => "snapshot",
    }
}

fn relation(repo: &Repository, ours: Option<ObjectId>, theirs: &str) -> HeadRelation {
    match theirs.parse::<ObjectId>() {
        Ok(theirs) => compare(repo, ours, theirs),
        Err(_) => HeadRelation::Unknown,
    }
}

/// How another member's head `theirs` compares with your branch, `ours`.
pub(crate) fn compare(repo: &Repository, ours: Option<ObjectId>, theirs: ObjectId) -> HeadRelation {
    let Some(ours) = ours else {
        return HeadRelation::Unknown;
    };
    if ours == theirs {
        return HeadRelation::Same;
    }
    let odb = repo.odb();
    if !odb.exists(&ours) || !odb.exists(&theirs) {
        return HeadRelation::Unknown;
    }
    match (
        repo.revwalk().is_ancestor(ours, theirs).unwrap_or(false),
        repo.revwalk().is_ancestor(theirs, ours).unwrap_or(false),
    ) {
        (true, _) => HeadRelation::Ahead,
        (_, true) => HeadRelation::Behind,
        _ => HeadRelation::Diverged,
    }
}

fn branch_json(here: Option<(&Repository, &str)>, share: SharedBranch) -> ShareBranchJson {
    let repo = here.map(|(repo, _)| repo);
    let ours = repo.and_then(|repo| {
        tips(repo, "refs/heads/")
            .ok()
            .and_then(|t| t.get(&share.branch).copied())
    });
    let branch = share.branch.clone();
    ShareBranchJson {
        members: share
            .members
            .into_iter()
            .map(|m| {
                let relation = match (repo, &m.head, m.you) {
                    (Some(repo), Some(head), false) => Some(relation(repo, ours, head)),
                    _ => None,
                };
                let divergence = match (here, relation, ours, m.head.as_deref()) {
                    (
                        Some((repo, remote)),
                        Some(HeadRelation::Diverged),
                        Some(ours),
                        Some(head),
                    ) => head
                        .parse::<ObjectId>()
                        .ok()
                        .and_then(|head| explain(repo, remote, &branch, &m.user, ours, head)),
                    _ => None,
                };
                (m, relation, divergence)
            })
            .map(|(m, relation, divergence)| ShareMemberJson {
                relation,
                divergence,
                user: m.user,
                sharer: m.sharer,
                you: m.you,
                head: m.head,
                project: m.project,
                snapshot: m.view.map(|v| v.paths),
            })
            .collect(),
        links: share
            .links
            .into_iter()
            .map(|l| ShareLinkJson {
                id: l.id,
                url: None,
                uses_left: l.uses_left,
                expires_at: l.expires_at,
                snapshot: l.view.map(|v| v.paths),
            })
            .collect(),
        invited: share.invited,
        branch: share.branch,
        origin: share.origin,
        sharer: share.sharer,
        state: state_word(share.state).to_owned(),
        mode: mode_word(share.mode).to_owned(),
        base_branch: share.base_branch,
        url: share.url,
    }
}

fn server_error(server: &ServerUrl, e: ServerAuthError) -> anyhow::Error {
    match e {
        ServerAuthError::TokenRejected => anyhow::anyhow!(
            "{} needs you signed in: run `grit login {}`",
            server.host(),
            server.root_url()
        ),
        ServerAuthError::NotGritServer(_) => anyhow::anyhow!(
            "{} doesn't share branches: sharing needs a grit hosting server such as grithub",
            server.host()
        ),
        other => anyhow::anyhow!("{other}"),
    }
}

/// The clone, set up against your space.
struct Here {
    repo: Repository,
    config: ConfigSet,
    server: ServerUrl,
    project: String,
    remote: String,
}

fn here() -> Result<Here> {
    let repo = context::discover()?;
    if crate::commands::checkout::open_checkout_here()?.is_some() {
        bail!("this is a snapshot checkout: share branches from a clone of the project");
    }
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    let Some(setup) = ensure_space(&repo, &config, "origin")? else {
        bail!("origin is not a project on a grit hosting server; sharing needs a clone of one, such as `grit clone https://grithub.dev/<owner>/<project>`");
    };
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    Ok(Here {
        repo,
        config,
        server: setup.server,
        project: setup.space.project,
        remote: setup.remote,
    })
}

fn ask(here: &Here, request: &ShareRequest) -> Result<ShareReply> {
    ask_about(&here.config, &here.server, &here.project, request)
}

/// Bring the shares' heads into the clone after a change.
fn refresh(here: &Here, reply: &ShareReply, mode: OutputMode) -> Result<()> {
    for share in &reply.shares {
        configure_shared(
            &here.repo,
            &here.remote,
            std::slice::from_ref(&share.branch),
            share.base_branch.as_deref(),
        )?;
    }
    fetch(&here.remote, mode)
}

fn outcome(here: &Here, action: &str, reply: ShareReply, pushed: bool) -> ShareOutcome {
    ShareOutcome {
        action: action.to_owned(),
        project: reply.project,
        link: reply.link.map(|l| ShareLinkJson {
            id: l.info.id,
            url: Some(l.url),
            uses_left: l.info.uses_left,
            expires_at: l.info.expires_at,
            snapshot: l.info.view.map(|v| v.paths),
        }),
        shares: reply
            .shares
            .into_iter()
            .map(|s| branch_json(Some((&here.repo, here.remote.as_str())), s))
            .collect(),
        added: reply.added,
        removed: reply.removed,
        pushed,
    }
}

/// Parse `--uses`: a number, or `unlimited`.
pub fn parse_uses(text: &str) -> Result<LinkUses> {
    if text.eq_ignore_ascii_case("unlimited") {
        return Ok(LinkUses::Unlimited);
    }
    match text.parse::<u32>() {
        Ok(0) | Err(_) => bail!("--uses takes a positive number or `unlimited`, not {text:?}"),
        Ok(n) => Ok(LinkUses::Count(n)),
    }
}

/// `grit share add`.
pub fn add(
    branch: &str,
    users: &[String],
    link: bool,
    uses: Option<&str>,
    expires: Option<u32>,
    view: Option<api::ShareView>,
    mode: OutputMode,
) -> Result<ShareOutcome> {
    if !link && (uses.is_some() || expires.is_some()) {
        bail!("--uses and --expires go with --link");
    }
    if !link && users.is_empty() {
        bail!("name someone to share {branch} with, or pass --link to make a link anyone signed in can accept");
    }
    let here = here()?;
    let local = tips(&here.repo, "refs/heads/")?.get(branch).copied();
    let remote = tips(&here.repo, &format!("refs/remotes/{}/", here.remote))?
        .get(branch)
        .copied();
    let pushed = match (local, remote) {
        (None, None) => bail!("there is no branch {branch} here or in your space"),
        (Some(local), remote) if Some(local) != remote => {
            if let Some(Some(why)) = push(&here.remote, &[branch.to_owned()], mode)?.get(branch) {
                bail!("pushing {branch} to your space first: {why}");
            }
            true
        }
        _ => false,
    };
    let options = match link {
        true => Some(LinkOptions {
            uses: uses.map(parse_uses).transpose()?.unwrap_or_default(),
            expires_days: expires.unwrap_or(api::DEFAULT_LINK_DAYS),
        }),
        false => None,
    };
    if let Some(days) = expires {
        if !(1..=api::MAX_LINK_DAYS).contains(&days) {
            bail!("--expires takes 1 to {} days", api::MAX_LINK_DAYS);
        }
    }
    let reply = ask(
        &here,
        &ShareRequest::Share {
            branch: branch.to_owned(),
            users: users.to_vec(),
            link: options,
            view,
        },
    )?;
    refresh(&here, &reply, mode)?;
    Ok(outcome(&here, "add", reply, pushed))
}

/// `grit share list`.
pub fn list(branch: Option<&str>, mode: OutputMode) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::List {
            branch: branch.map(str::to_owned),
        },
    )?;
    if !reply.shares.is_empty() {
        refresh(&here, &reply, mode)?;
    }
    Ok(outcome(&here, "list", reply, false))
}

/// `grit share remove`.
pub fn remove(branch: &str, users: &[String], mode: OutputMode) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::Unshare {
            branch: branch.to_owned(),
            users: users.to_vec(),
        },
    )?;
    if users.is_empty() {
        configure_shared(&here.repo, &here.remote, &[], None)?;
    }
    fetch(&here.remote, mode)?;
    Ok(outcome(&here, "remove", reply, false))
}

/// `grit share leave`.
pub fn leave(branch: &str, mode: OutputMode) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::Leave {
            branch: branch.to_owned(),
        },
    )?;
    crate::commands::sync::unmark_shared(&here.repo, branch)?;
    fetch(&here.remote, mode)?;
    Ok(outcome(&here, "leave", reply, false))
}

/// `grit share revoke`.
pub fn revoke(branch: &str, link: &str) -> Result<ShareOutcome> {
    let here = here()?;
    let reply = ask(
        &here,
        &ShareRequest::RevokeLink {
            branch: branch.to_owned(),
            link: link.to_owned(),
        },
    )?;
    Ok(outcome(&here, "revoke", reply, false))
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

/// `grit share resolve`.
pub fn resolve(
    branch: &str,
    option: Option<&str>,
    name: Option<&str>,
    with: Option<&str>,
    mode: OutputMode,
) -> Result<ShareResolveOutcome> {
    let here = here()?;
    fetch(&here.remote, mode)?;
    let Some(ours) = tips(&here.repo, "refs/heads/")?.get(branch).copied() else {
        bail!("there is no branch {branch} here");
    };
    let heads = crate::commands::sync::shared_heads(&here.repo, &here.remote, branch)?;
    let integrated_option = option.is_some_and(|o| INTEGRATED_OPTIONS.contains(&o));
    if heads.is_empty() || integrated_option {
        if let Some(report) = integrated_report(&here, branch, ours, !integrated_option)? {
            return resolve_integrated(&here, branch, ours, report, option, name, mode);
        }
        if integrated_option {
            bail!(
                "{branch} wasn't shared and integrated lately, so there's nothing to {}",
                option.unwrap_or_default()
            );
        }
        bail!("{branch} isn't shared here, or nobody else has pushed a head of it yet");
    }
    let diverged: Vec<(String, ObjectId)> = heads
        .into_iter()
        .filter(|(user, tip)| {
            with.is_none_or(|w| w == user)
                && compare(&here.repo, Some(ours), *tip) == HeadRelation::Diverged
        })
        .collect();
    let (user, theirs) = match (diverged.as_slice(), with) {
        ([one], _) => one.clone(),
        ([], Some(w)) => {
            bail!("{w}'s head of {branch} hasn't diverged from yours: `grit sync` brings it in")
        }
        ([], None) => {
            bail!("nobody's head of {branch} has diverged from yours: `grit sync` brings theirs in")
        }
        (many, None) => bail!(
            "several heads of {branch} diverged from yours ({}): name one with --with <user>",
            many.iter()
                .map(|(u, _)| u.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (_, Some(w)) => bail!("{w} isn't on {branch}"),
    };
    let report = explain(&here.repo, &here.remote, branch, &user, ours, theirs)
        .context("comparing the two heads")?;
    let mut outcome = ShareResolveOutcome {
        branch: branch.to_owned(),
        with: user.clone(),
        scenario: report.scenario.clone(),
        summary: report.summary.clone(),
        option: None,
        previous: ours.to_hex(),
        head: None,
        pushed: false,
        next: None,
        options: Vec::new(),
        renamed: None,
        shared_with: Vec::new(),
    };
    let chosen = match option {
        Some(id) => id.to_owned(),
        None if !report.needs_decision => "combine".to_owned(),
        None => {
            outcome.options = report.options;
            return Ok(outcome);
        }
    };
    let Some(choice) = grit_lib::divergence::Choice::parse(&chosen) else {
        let ids: Vec<&str> = report.options.iter().map(|o| o.id.as_str()).collect();
        bail!(
            "{chosen:?} isn't an option here; choose one of: {}",
            ids.join(", ")
        );
    };
    if !report.options.iter().any(|o| o.id == chosen) {
        let ids: Vec<&str> = report.options.iter().map(|o| o.id.as_str()).collect();
        bail!(
            "{chosen} isn't an option for {} ({}); choose one of: {}",
            report.scenario,
            report.summary,
            ids.join(", ")
        );
    }
    outcome.option = Some(chosen);
    if choice == grit_lib::divergence::Choice::Resolve {
        outcome.next = Some(format!(
            "grit rebase {}/shared/{branch}/{user}, settle the conflicts and `grit rebase --continue`, then `grit push`",
            here.remote
        ));
        return Ok(outcome);
    }
    let head = match put_together(&here.repo, &here.remote, branch, &user, ours, theirs, choice)? {
        Ok(head) => head,
        Err(why) => bail!(
            "{why}: choose another option, or resolve it by hand with `grit share resolve {branch} resolve --with {user}`"
        ),
    };
    if head != ours {
        move_branch(&here.repo, branch, head)?;
    }
    outcome.head = Some(head.to_hex());
    if let Some(Some(why)) = push(&here.remote, &[branch.to_owned()], mode)?.get(branch) {
        bail!(
            "{branch} is at {} here, but pushing it was refused: {why}",
            short_hex(&head.to_hex())
        );
    }
    outcome.pushed = true;
    Ok(outcome)
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

/// The `grit share resolve` options for a branch pushed again after its
/// share was integrated.
const INTEGRATED_OPTIONS: [&str; 3] = ["rebase", "rename", "reshare"];

/// A branch pushed (or synced) again after its share was integrated,
/// bringing integrated commits: scenario `pushed_to_integrated`. It is a
/// branch nobody shares now.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IntegratedJson {
    /// Always `pushed_to_integrated`.
    pub scenario: String,
    /// What happened, in a sentence.
    pub summary: String,
    /// Who integrated it.
    pub by: String,
    /// When, RFC 3339.
    pub at: String,
    /// The project it was merged into, `<owner>/<project>`.
    pub into: String,
    /// The branch it was merged into.
    pub target_branch: String,
    /// The pull request it went through, for an organization's project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull: Option<u64>,
    /// The pull request's page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// How many of the branch's commits were integrated already.
    pub integrated: usize,
    /// How many are new.
    pub new: usize,
    /// The ways forward, recommended first.
    pub options: Vec<ResolveOptionJson>,
}

impl IntegratedJson {
    /// The report, for people: what happened and the options.
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

    /// The report, for agents.
    pub(crate) fn render_markdown(&self, w: &mut dyn Write, branch: &str) -> Result<()> {
        outln!(w, "**Scenario:** `{}`. {}.", self.scenario, self.summary);
        if let Some(url) = &self.url {
            outln!(w);
            outln!(w, "Pull request: {url}");
        }
        outln!(w);
        outln!(w, "`{branch}` is a branch nobody shares now. Options:");
        outln!(w);
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
        Ok(())
    }
}

/// Ask `server` about the shares of `branch` in `project`.
fn ask_about(
    config: &ConfigSet,
    server: &ServerUrl,
    project: &str,
    request: &ShareRequest,
) -> Result<ShareReply> {
    let http = context::http_client_factory()
        .create(config)
        .context("could not set up HTTP client")?;
    let reply =
        api::share(http.as_ref(), server, project, request).map_err(|e| server_error(server, e))?;
    if let Some(why) = &reply.refused {
        bail!("{why}");
    }
    Ok(reply)
}

/// When `branch` of `project` on `server` (here at `tip`) brings commits of
/// a share of it integrated lately: the report. `None` when it wasn't
/// shared and integrated, or brings none of the commits integrated (reusing
/// the name for other work).
pub(crate) fn integrated_on(
    repo: &Repository,
    config: &ConfigSet,
    server: &ServerUrl,
    project: &str,
    branch: &str,
    tip: ObjectId,
) -> Result<Option<IntegratedJson>> {
    integrated_share(repo, config, server, project, branch, tip, true)
}

/// [`integrated_on`], or with `carried` unset, the report for any share of
/// `branch` integrated lately, whatever the branch holds now.
fn integrated_share(
    repo: &Repository,
    config: &ConfigSet,
    server: &ServerUrl,
    project: &str,
    branch: &str,
    tip: ObjectId,
    carried: bool,
) -> Result<Option<IntegratedJson>> {
    let reply = ask_about(
        config,
        server,
        project,
        &ShareRequest::List {
            branch: Some(branch.to_owned()),
        },
    )?;
    if reply
        .shares
        .iter()
        .any(|s| s.branch == branch && s.state == ShareState::Active)
    {
        return Ok(None);
    }
    let Some((share, how)) = reply.shares.iter().find_map(|s| {
        (s.branch == branch && s.state == ShareState::Integrated)
            .then_some(s)
            .and_then(|s| s.integrated.as_ref().map(|i| (s, i)))
    }) else {
        return Ok(None);
    };
    let keys: std::collections::HashSet<&str> = how.changes.iter().map(String::as_str).collect();
    if keys.is_empty() && carried {
        return Ok(None);
    }
    let commits = repo.revwalk().keyed_commits(tip, &[])?;
    let integrated = commits
        .iter()
        .filter(|k| keys.contains(k.key.as_str()))
        .count();
    if integrated == 0 && carried {
        return Ok(None);
    }
    let new = commits
        .iter()
        .rposition(|k| keys.contains(k.key.as_str()))
        .map_or(0, |i| commits.len() - i - 1);
    let others: Vec<String> = share
        .members
        .iter()
        .filter(|m| !m.you)
        .map(|m| m.user.clone())
        .collect();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let summary = format!(
        "{branch} was shared, and {} integrated it into {} {}; {integrated} of its commit{} {} there already, {new} {} new",
        how.by,
        how.into,
        how.target_branch,
        plural(integrated),
        if integrated == 1 { "is" } else { "are" },
        if new == 1 { "is" } else { "are" },
    );
    let renamed = format!("{branch}-2");
    let mut options = vec![
        ResolveOptionJson {
            id: "rebase".to_owned(),
            label: format!(
                "rebase onto {}: keep only the {new} new commit{}",
                how.target_branch,
                plural(new)
            ),
            loses: None,
            command: format!("grit share resolve {branch} rebase"),
        },
        ResolveOptionJson {
            id: "rename".to_owned(),
            label: format!("keep it as {renamed}"),
            loses: None,
            command: format!("grit share resolve {branch} rename {renamed}"),
        },
    ];
    if !others.is_empty() {
        options.push(ResolveOptionJson {
            id: "reshare".to_owned(),
            label: format!("share it again with {}", others.join(", ")),
            loses: None,
            command: format!("grit share resolve {branch} reshare"),
        });
    }
    Ok(Some(IntegratedJson {
        scenario: "pushed_to_integrated".to_owned(),
        summary,
        by: how.by.clone(),
        at: how.at.clone(),
        into: how.into.clone(),
        target_branch: how.target_branch.clone(),
        pull: how.pull,
        url: how.url.clone(),
        integrated,
        new,
        options,
    }))
}

fn integrated_report(
    here: &Here,
    branch: &str,
    tip: ObjectId,
    carried: bool,
) -> Result<Option<IntegratedJson>> {
    integrated_share(
        &here.repo,
        &here.config,
        &here.server,
        &here.project,
        branch,
        tip,
        carried,
    )
}

/// `grit share resolve <branch> rebase|rename <name>|reshare`, for a
/// branch pushed again after its share was integrated.
fn resolve_integrated(
    here: &Here,
    branch: &str,
    ours: ObjectId,
    report: IntegratedJson,
    option: Option<&str>,
    name: Option<&str>,
    mode: OutputMode,
) -> Result<ShareResolveOutcome> {
    let mut outcome = ShareResolveOutcome {
        branch: branch.to_owned(),
        with: report.by.clone(),
        scenario: report.scenario.clone(),
        summary: report.summary.clone(),
        option: None,
        previous: ours.to_hex(),
        head: None,
        pushed: false,
        next: None,
        options: Vec::new(),
        renamed: None,
        shared_with: Vec::new(),
    };
    let Some(option) = option else {
        outcome.options = report.options;
        return Ok(outcome);
    };
    if !report.options.iter().any(|o| o.id == option) {
        let ids: Vec<&str> = report.options.iter().map(|o| o.id.as_str()).collect();
        bail!(
            "{option} isn't an option for {} ({}); choose one of: {}",
            report.scenario,
            report.summary,
            ids.join(", ")
        );
    }
    outcome.option = Some(option.to_owned());
    match option {
        "rebase" => {
            fetch("origin", mode)?;
            let target = &report.target_branch;
            let onto = tips(&here.repo, "refs/remotes/origin/")?
                .get(target)
                .copied()
                .or(tips(&here.repo, &format!("refs/remotes/{}/", here.remote))?
                    .get(target)
                    .copied())
                .with_context(|| format!("there is no origin/{target} here to rebase onto"))?;
            let reply = ask_about(
                &here.config,
                &here.server,
                &here.project,
                &ShareRequest::List {
                    branch: Some(branch.to_owned()),
                },
            )?;
            let skip: std::collections::HashSet<String> = reply
                .shares
                .iter()
                .filter_map(|s| s.integrated.as_ref())
                .flat_map(|i| i.changes.iter().cloned())
                .collect();
            let head = match here.repo.revwalk().replay_onto(ours, &[onto], onto, &skip)? {
                grit_lib::divergence::ReplayOntoOutcome::Ok { tip } => tip,
                grit_lib::divergence::ReplayOntoOutcome::Conflict { paths } => bail!(
                    "rebasing {branch} onto {target} conflicts in {}: run `grit rebase origin/{target}` and settle them by hand",
                    paths
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            if head != ours {
                move_branch(&here.repo, branch, head)?;
            }
            outcome.head = Some(head.to_hex());
            let spec = format!("+refs/heads/{branch}:refs/heads/{branch}");
            let pushed = crate::commands::sync::push_specs(&here.remote, &[spec], mode)?;
            if let Some(Some(why)) = pushed.get(branch) {
                bail!(
                    "{branch} is at {} here, but pushing it was refused: {why}",
                    short_hex(&head.to_hex())
                );
            }
            outcome.pushed = true;
        }
        "rename" => {
            let Some(new) = name else {
                bail!("name the new branch: `grit share resolve {branch} rename <name>`");
            };
            grit_lib::porcelain::branch::rename_branch(
                &here.repo,
                &grit_lib::porcelain::branch::RenameBranchOptions {
                    old: branch.to_owned(),
                    new: new.to_owned(),
                    force: false,
                    dry_run: false,
                },
            )
            .with_context(|| format!("renaming {branch} to {new}"))?;
            let specs = vec![
                format!("refs/heads/{new}:refs/heads/{new}"),
                format!(":refs/heads/{branch}"),
            ];
            let pushed = crate::commands::sync::push_specs(&here.remote, &specs, mode)?;
            if let Some(Some(why)) = pushed.get(new) {
                bail!("{new} was renamed here, but pushing it was refused: {why}");
            }
            outcome.renamed = Some(new.to_owned());
            outcome.pushed = true;
        }
        _ => {
            let reply = ask_about(
                &here.config,
                &here.server,
                &here.project,
                &ShareRequest::Reshare {
                    branch: branch.to_owned(),
                },
            )?;
            let share = reply.shares.iter().find(|s| s.branch == branch);
            configure_shared(
                &here.repo,
                &here.remote,
                &[branch.to_owned()],
                share.and_then(|s| s.base_branch.as_deref()),
            )?;
            outcome.shared_with = share
                .map(|s| {
                    s.members
                        .iter()
                        .filter(|m| !m.you)
                        .map(|m| m.user.clone())
                        .collect()
                })
                .unwrap_or_default();
        }
    }
    Ok(outcome)
}

/// `grit share accept`.
pub fn accept(link: &str, mode: OutputMode) -> Result<ShareAcceptOutcome> {
    let Some((server, _token)) = api::share_link(link) else {
        bail!("{link} isn't a share link: they look like https://grithub.dev/share/<token>");
    };
    let repo = context::discover().ok();
    let env = context::environment();
    let config = ConfigSet::load(
        &env,
        repo.as_ref().map(Repository::git_dir),
        &grit_lib::config::LoadConfigOptions::standard(
            &env,
            repo.as_ref().map(Repository::git_dir),
        ),
    )
    .context("could not load config")?;
    let http = context::http_client_factory()
        .create(&config)
        .context("could not set up HTTP client")?;
    let reply = api::accept(
        http.as_ref(),
        &server,
        &ShareAcceptRequest {
            link: link.to_owned(),
        },
    )
    .map_err(|e| server_error(&server, e))?;
    if reply.status == ShareAcceptStatus::Refused {
        bail!(
            "{}",
            reply
                .message
                .unwrap_or_else(|| "the link can't be accepted".to_owned())
        );
    }
    let (Some(share), Some(project), Some(url)) = (reply.share, reply.project, reply.url) else {
        bail!("{} answered without the share", server.host());
    };
    let status = match reply.status {
        ShareAcceptStatus::AlreadyMember => "already_member",
        _ => "accepted",
    };
    // In a clone of the project (or of the organization's project it
    // copies), fetch into it; anywhere else, clone your copy.
    let in_clone = repo.as_ref().and_then(|repo| {
        let url = crate::context::config_string(&config, "remote.origin.url")?;
        let (origin_server, origin_project) =
            grit_lib::remote::grit_server_space::hosted_project(&url)?;
        let name = |p: &str| p.split_once('/').map(|(_, n)| n.to_owned());
        (origin_server.root_url() == server.root_url() && name(&origin_project) == name(&project))
            .then_some(repo)
    });
    let (path, cloned) = match in_clone {
        Some(repo) => {
            let config = ConfigSet::load(
                &context::environment(),
                Some(repo.git_dir()),
                &grit_lib::config::LoadConfigOptions::standard(
                    &context::environment(),
                    Some(repo.git_dir()),
                ),
            )
            .context("could not load config")?;
            let Some(setup) = ensure_space(repo, &config, "origin")? else {
                bail!("origin is not a project on {}", server.host());
            };
            configure_shared(
                repo,
                &setup.remote,
                std::slice::from_ref(&share.branch),
                share.base_branch.as_deref(),
            )?;
            fetch(&setup.remote, mode)?;
            let dir = repo
                .work_tree()
                .map(grit_lib::config::git_path::user_display_path)
                .unwrap_or_else(|| grit_lib::config::git_path::user_display_path(repo.git_dir()));
            (Some(dir), false)
        }
        None => {
            let cloned = crate::commands::clone::run(crate::commands::clone::CloneRunOptions {
                url: url.clone(),
                dir: None,
                depth: None,
                branch: Some(share.branch.clone()),
                shallow: false,
                single_branch: Some(false),
                no_tags: false,
                origin: None,
                bare: false,
                no_checkout: false,
                blobs: None,
                config: Vec::new(),
                quiet: crate::quiet_args::QuietArgs { quiet: true },
                mode,
            })
            .with_context(|| format!("cloning {url}"))?;
            // grit runs from where it started, so open the new clone by path.
            let (options, _sink) = crate::diagnostics::repository_options(false);
            let repo = Repository::discover(Some(std::path::Path::new(&cloned.path)), &options)
                .with_context(|| format!("opening {}", cloned.path))?;
            configure_shared(
                &repo,
                "origin",
                std::slice::from_ref(&share.branch),
                share.base_branch.as_deref(),
            )?;
            crate::commands::sync::fetch_in(&repo, "origin", mode)?;
            (Some(cloned.path), true)
        }
    };
    Ok(ShareAcceptOutcome {
        status: status.to_owned(),
        branch: share.branch,
        sharer: share.sharer,
        origin: share.origin,
        project,
        url,
        path,
        cloned,
    })
}

/// How a snapshot share's paths read: `snapshot of src/rpc`, or `snapshot`
/// for the whole tree.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_are_a_number_or_unlimited() {
        assert_eq!(parse_uses("3").unwrap(), LinkUses::Count(3));
        assert_eq!(parse_uses("Unlimited").unwrap(), LinkUses::Unlimited);
        assert!(parse_uses("0").is_err());
        assert!(parse_uses("many").is_err());
    }
}

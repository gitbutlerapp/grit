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
use grit_lib::remote::grit_server_auth::{ServerAuthError, ServerUrl};
use grit_lib::remote::grit_server_share::{
    self as api, LinkOptions, LinkUses, ShareAcceptRequest, ShareAcceptStatus, ShareMode,
    ShareReply, ShareRequest, ShareState, SharedBranch,
};
use grit_lib::remote::HttpClientFactory as _;
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::is_ancestor;
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
}

impl DivergenceJson {
    pub(crate) fn from_divergence(d: &grit_lib::divergence::Divergence, them: &str) -> Self {
        Self {
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
            conflicts: d.conflicts.clone(),
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
        if self.needs_decision {
            outln!(
                w,
                "{indent}  Someone has to decide what to do: nothing is lost either way, since every head stays in the share's log."
            );
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
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true).ok()?;
    let mut floor = Vec::new();
    if let Some(base) = config.get(&format!("branch.{branch}.sharedBase")) {
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
    let seen = |name: &str| grit_lib::divergence::heads_seen(repo, name);
    let ours_seen = seen(&format!("refs/heads/{branch}"));
    let theirs_seen = seen(&format!("refs/remotes/{remote}/shared/{branch}/{user}"));
    let base =
        grit_lib::divergence::common_base(repo, ours, theirs, &ours_seen, &theirs_seen).ok()?;
    let d = grit_lib::divergence::classify(repo, &floor, base, ours, theirs).ok()?;
    Some(DivergenceJson::from_divergence(&d, user))
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
        is_ancestor(repo, ours, theirs).unwrap_or(false),
        is_ancestor(repo, theirs, ours).unwrap_or(false),
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
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let Some(setup) = ensure_space(&repo, &config, "origin")? else {
        bail!("origin is not a project on a grit hosting server; sharing needs a clone of one, such as `grit clone https://grithub.dev/<owner>/<project>`");
    };
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
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
    let http = context::http_client_factory()
        .create(&here.config)
        .context("could not set up HTTP client")?;
    let reply = api::share(http.as_ref(), &here.server, &here.project, request)
        .map_err(|e| server_error(&here.server, e))?;
    if let Some(why) = &reply.refused {
        bail!("{why}");
    }
    Ok(reply)
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

/// `grit share accept`.
pub fn accept(link: &str, mode: OutputMode) -> Result<ShareAcceptOutcome> {
    let Some((server, _token)) = api::share_link(link) else {
        bail!("{link} isn't a share link: they look like https://grithub.dev/share/<token>");
    };
    let repo = context::discover().ok();
    let config = ConfigSet::load(
        &context::environment(),
        repo.as_ref().map(Repository::git_dir),
        true,
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
        let url = config.get("remote.origin.url")?;
        let (origin_server, origin_project) =
            grit_lib::remote::grit_server_space::hosted_project(&url)?;
        let name = |p: &str| p.split_once('/').map(|(_, n)| n.to_owned());
        (origin_server.root_url() == server.root_url() && name(&origin_project) == name(&project))
            .then_some(repo)
    });
    let (path, cloned) = match in_clone {
        Some(repo) => {
            let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
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
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| repo.git_dir().display().to_string());
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
        outln!(w, "  link {}  {uses}, until {}", l.id, l.expires_at);
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
            outln!(w, "✓ share link ({uses}, until {}):", link.expires_at);
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
            outln!(
                w,
                "- Share link (uses: {uses}, until {}): {}",
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
                outln!(w, "| Link | Uses left | Expires |");
                outln!(w, "| --- | --- | --- |");
                for l in &s.links {
                    let uses = l
                        .uses_left
                        .map_or_else(|| "unlimited".to_owned(), |n| n.to_string());
                    outln!(w, "| `{}` | {uses} | {} |", l.id, l.expires_at);
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

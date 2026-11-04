//! `grit sync` — keep a clone of a hosted project and your space on the
//! server in step.
//!
//! On a grit hosting server such as grithub, your work on a project lives in
//! your space (`<you>/<project>`). For a clone of an organization's project,
//! the server keeps a copy there that follows the organization's branches;
//! sync sets the clone up to push to it (`remote.origin.pushurl`, and a
//! `space` remote to fetch from). Then, on every run, it:
//!
//! 1. fetches the project and your space;
//! 2. fast-forwards the target branch (usually `main`) to the project's;
//! 3. brings down branches of your space: creating the ones you don't have
//!    here, fast-forwarding the ones that are behind;
//! 4. pushes every active branch to your space: not the target, not one the
//!    space follows from upstream (read-only there), and not already merged
//!    into the target.
//!
//! A branch that has diverged from your space's is left alone and reported.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::config::{set_config_value, ConfigLocation};
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::remote::grit_server_auth::{ServerAuthError, ServerUrl};
use grit_lib::remote::grit_server_space::{fetch_space, hosted_project, Space};
use grit_lib::remote::HttpClientFactory as _;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::RemoteUrlKind;
use grit_lib::revwalk::merge_base::is_ancestor;
use grit_lib::state::{resolve_head_for_repository, HeadState};
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::fetch_args::FetchArgs;
use crate::commands::merge::MergeRunOptions;
use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::quiet_args::QuietArgs;

/// The remote a clone of an organization's project fetches your space from.
pub const SPACE_REMOTE: &str = "space";

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

impl TargetAction {
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

impl SyncAction {
    fn word(self) -> &'static str {
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

/// A clone set up against your space.
pub(crate) struct SpaceSetup {
    pub server: ServerUrl,
    pub space: Space,
    /// Where your space is fetched from and pushed to: `origin` when it is
    /// your space, else [`SPACE_REMOTE`].
    pub remote: String,
    /// Whether this call changed the clone's config.
    pub configured: bool,
}

/// Ask the server behind `remote` where your work on its project goes, and
/// set the clone up for it: for a project not in your space, a
/// [`SPACE_REMOTE`] remote and `remote.<remote>.pushurl`, so plain pushes go
/// to your space.
///
/// # Returns
///
/// `None` when the remote's URL does not name a project on an `http(s)`
/// host.
///
/// # Errors
///
/// When the server can't be reached, has no space endpoint, refuses the token
/// or the config can't be written.
pub(crate) fn ensure_space(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
) -> Result<Option<SpaceSetup>> {
    let Some(url) = config.get(&format!("remote.{remote}.url")) else {
        return Ok(None);
    };
    let Some((server, project)) = hosted_project(&url) else {
        return Ok(None);
    };
    let http = context::http_client_factory()
        .create(config)
        .context("could not set up HTTP client")?;
    let space =
        fetch_space(http.as_ref(), &server, &project).map_err(|e| space_error(&server, e))?;
    let mut configured = false;
    let space_remote = if space.project == project {
        remote.to_owned()
    } else {
        let remotes = repo.remotes();
        match config.get(&format!("remote.{SPACE_REMOTE}.url")) {
            Some(existing) if existing == space.url => {}
            Some(_) => {
                remotes
                    .set_url(SPACE_REMOTE, &space.url, RemoteUrlKind::Fetch, false)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                configured = true;
            }
            None => {
                remotes
                    .add(SPACE_REMOTE, &space.url)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                configured = true;
            }
        }
        if config.get(&format!("remote.{remote}.pushurl")).as_deref() != Some(space.url.as_str()) {
            remotes
                .set_url(remote, &space.url, RemoteUrlKind::Push, false)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            configured = true;
        }
        SPACE_REMOTE.to_owned()
    };
    Ok(Some(SpaceSetup {
        server,
        space,
        remote: space_remote,
        configured,
    }))
}

fn space_error(server: &ServerUrl, e: ServerAuthError) -> anyhow::Error {
    match e {
        ServerAuthError::TokenRejected => anyhow::anyhow!(
            "{} needs you signed in: run `grit login {}`",
            server.host(),
            server.root_url()
        ),
        ServerAuthError::NotGritServer(_) => anyhow::anyhow!(
            "{} has no space for this project: sync works with projects on a grit hosting server such as grithub, that you can read",
            server.host()
        ),
        other => anyhow::anyhow!("{other}"),
    }
}

/// Branch name → tip, for refs under `prefix`.
pub(crate) fn tips(repo: &Repository, prefix: &str) -> Result<BTreeMap<String, ObjectId>> {
    Ok(grit_lib::refs::list_refs(repo.git_dir(), prefix)?
        .into_iter()
        .filter_map(|(name, oid)| Some((name.strip_prefix(prefix)?.to_owned(), oid)))
        .collect())
}

pub(crate) fn fetch(remote: &str, mode: OutputMode) -> Result<()> {
    let args = FetchArgs {
        remote: Some(remote.to_owned()),
        quiet: QuietArgs { quiet: true },
        ..FetchArgs::default()
    };
    crate::commands::fetch::run(&args, mode)
        .with_context(|| format!("fetching {remote}"))
        .map(|_| ())
}

/// The branch `HEAD` is on, if any.
fn current_branch(repo: &Repository) -> Option<String> {
    match resolve_head_for_repository(repo).ok()? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    }
}

/// Move branch `name` forward to `to`: through a fast-forward merge when it
/// is checked out, so the working tree follows, else by setting the ref.
fn fast_forward(repo: &Repository, name: &str, to: &str, oid: ObjectId) -> Result<()> {
    if current_branch(repo).as_deref() == Some(name) {
        let mut progress = grit_lib::progress::NullProgress;
        crate::commands::merge::run(
            Some(to),
            false,
            false,
            false,
            false,
            MergeRunOptions {
                ff_only: true,
                ..MergeRunOptions::default()
            },
            &mut progress,
        )
        .with_context(|| format!("fast-forwarding {name}"))?;
        Ok(())
    } else {
        grit_lib::refs::write_ref(repo.git_dir(), &format!("refs/heads/{name}"), &oid)
            .with_context(|| format!("moving {name}"))
    }
}

fn set_tracking(repo: &Repository, branch: &str, remote: &str) -> Result<()> {
    let env = context::environment();
    for (key, value) in [
        (format!("branch.{branch}.remote"), remote.to_owned()),
        (
            format!("branch.{branch}.merge"),
            format!("refs/heads/{branch}"),
        ),
    ] {
        set_config_value(
            ConfigLocation::Local,
            &env,
            Some(repo.git_dir()),
            &key,
            &value,
            false,
        )
        .with_context(|| format!("setting {key}"))?;
    }
    Ok(())
}

/// Push `branches` to `remote`; each branch's result.
pub(crate) fn push(
    remote: &str,
    branches: &[String],
    mode: OutputMode,
) -> Result<BTreeMap<String, Option<String>>> {
    if branches.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut positionals = vec![remote.to_owned()];
    positionals.extend(
        branches
            .iter()
            .map(|b| format!("refs/heads/{b}:refs/heads/{b}")),
    );
    let outcome = crate::commands::push::run(
        false,
        false,
        None,
        &positionals,
        false,
        false,
        None,
        false,
        false,
        &[],
        false,
        &QuietArgs { quiet: true },
        mode,
        false,
    )?;
    Ok(outcome
        .results
        .into_iter()
        .filter_map(|r| {
            let name = r.ref_name.strip_prefix("refs/heads/")?.to_owned();
            let refused = matches!(
                r.status,
                crate::commands::push::PushRefOutcomeStatus::Rejected
            )
            .then(|| r.reason.unwrap_or_else(|| "rejected".to_owned()));
            Some((name, refused))
        })
        .collect())
}

/// Bring the target branch here up to the project's.
pub(crate) fn sync_target(
    repo: &Repository,
    upstream_remote: &str,
    target: &str,
) -> Result<Option<SyncTarget>> {
    let Some(theirs) = tips(repo, &format!("refs/remotes/{upstream_remote}/"))?
        .get(target)
        .copied()
    else {
        return Ok(None);
    };
    let ours = tips(repo, "refs/heads/")?.get(target).copied();
    let action = match ours {
        None => {
            grit_lib::refs::write_ref(repo.git_dir(), &format!("refs/heads/{target}"), &theirs)?;
            TargetAction::Created
        }
        Some(ours) if ours == theirs => TargetAction::UpToDate,
        Some(ours) if is_ancestor(repo, ours, theirs)? => {
            fast_forward(repo, target, &format!("{upstream_remote}/{target}"), theirs)?;
            TargetAction::FastForwarded
        }
        Some(ours) if is_ancestor(repo, theirs, ours)? => TargetAction::Ahead,
        Some(_) => TargetAction::Diverged,
    };
    let tip = tips(repo, "refs/heads/")?.get(target).map(ObjectId::to_hex);
    Ok(Some(SyncTarget {
        name: target.to_owned(),
        action,
        tip,
    }))
}

/// `grit sync`: see the module docs.
pub fn run(mode: OutputMode) -> Result<SyncOutcome> {
    let repo = context::discover()?;
    if crate::commands::checkout::open_checkout_here()?.is_some() {
        bail!("this is a snapshot checkout: send your branches with `grit checkin` and update with `grit pull`");
    }
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let Some(setup) = ensure_space(&repo, &config, "origin")? else {
        bail!("origin is not a project on a grit hosting server; sync needs a clone of one, such as `grit clone https://grithub.dev/<owner>/<project>`");
    };

    fetch("origin", mode)?;
    if setup.remote != "origin" {
        fetch(&setup.remote, mode)?;
    }

    let target_name = setup
        .space
        .target_branch
        .clone()
        .unwrap_or_else(|| "main".to_owned());
    let target = sync_target(&repo, "origin", &target_name)?;
    let base = tips(&repo, "refs/remotes/origin/")?
        .get(&target_name)
        .copied();

    let followed: BTreeSet<&str> = setup.space.followed.iter().map(String::as_str).collect();
    let local = tips(&repo, "refs/heads/")?;
    let remote = tips(&repo, &format!("refs/remotes/{}/", setup.remote))?;
    let names: BTreeSet<&String> = local
        .keys()
        .chain(remote.keys())
        .filter(|n| **n != target_name && !followed.contains(n.as_str()) && n.as_str() != "HEAD")
        .collect();

    let merged = |oid: ObjectId| -> bool {
        base.is_some_and(|base| is_ancestor(&repo, oid, base).unwrap_or(false))
    };
    let mut branches = Vec::new();
    let mut to_push = Vec::new();
    for name in names {
        let (ours, theirs) = (local.get(name).copied(), remote.get(name).copied());
        let action = match (ours, theirs) {
            (Some(ours), None) if merged(ours) => continue,
            (None, Some(theirs)) if merged(theirs) => continue,
            (Some(ours), Some(theirs)) if ours == theirs => {
                if merged(ours) {
                    continue;
                }
                SyncAction::UpToDate
            }
            (Some(_), None) => {
                to_push.push(name.clone());
                SyncAction::Pushed
            }
            (None, Some(theirs)) => {
                grit_lib::refs::write_ref(repo.git_dir(), &format!("refs/heads/{name}"), &theirs)?;
                set_tracking(&repo, name, &setup.remote)?;
                SyncAction::Created
            }
            (Some(ours), Some(theirs)) if is_ancestor(&repo, ours, theirs)? => {
                fast_forward(&repo, name, &format!("{}/{name}", setup.remote), theirs)?;
                SyncAction::Pulled
            }
            (Some(ours), Some(theirs)) if is_ancestor(&repo, theirs, ours)? => {
                to_push.push(name.clone());
                SyncAction::Pushed
            }
            (Some(_), Some(_)) => SyncAction::Diverged,
            (None, None) => continue,
        };
        let message = (action == SyncAction::Diverged).then(|| {
            format!(
                "{name} and {}/{name} each have commits the other lacks: switch to it and run `grit pull --rebase`, then sync again",
                setup.remote
            )
        });
        branches.push(SyncBranch {
            name: name.clone(),
            action,
            tip: None,
            message,
        });
    }

    let pushed = push(&setup.remote, &to_push, mode)?;
    let local = tips(&repo, "refs/heads/")?;
    for branch in &mut branches {
        if let Some(Some(why)) = pushed.get(&branch.name) {
            branch.action = SyncAction::Refused;
            branch.message = Some(why.clone());
        }
        branch.tip = local.get(&branch.name).map(ObjectId::to_hex);
    }

    Ok(SyncOutcome {
        space: setup.space.project,
        upstream: setup.space.upstream,
        remote: setup.remote,
        configured: setup.configured,
        target,
        branches,
    })
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
        }
        Ok(())
    }
}

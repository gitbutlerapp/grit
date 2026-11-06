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
use grit_lib::config::{ConfigScope, ConfigWriteOptions};
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::remote::grit_server_auth::{ServerAuthError, ServerUrl};
use grit_lib::remote::grit_server_share::SHARED_REFS;
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
    /// Whether a shared branch's heads diverged in a way someone has to
    /// decide about.
    pub fn needs_decision(&self) -> bool {
        self.branches
            .iter()
            .any(|b| b.integrated.is_some() || b.shared.iter().any(SharedHead::needs_decision))
    }

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
    /// For a shared branch: the other members' heads, compared with yours
    /// afterwards.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub shared: Vec<SharedHead>,
    /// For a branch this sync made again after a share of it was
    /// integrated (scenario `pushed_to_integrated`): what happened, and the
    /// options. Nobody shares it now.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub integrated: Option<crate::commands::share::IntegratedJson>,
}

/// Another member's head of a shared branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SharedHead {
    /// Their user name.
    pub user: String,
    /// Their head's tip.
    pub tip: String,
    /// How it compares with your branch here.
    pub relation: crate::commands::share::HeadRelation,
    /// For a head that diverged from yours: what happened, and whether
    /// someone has to decide what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub divergence: Option<crate::commands::share::DivergenceJson>,
}

impl SharedHead {
    /// The head `tip` of `user`, compared with `ours` (and explained when
    /// they diverged).
    pub(crate) fn new(
        repo: &Repository,
        remote: &str,
        branch: &str,
        user: String,
        ours: Option<ObjectId>,
        tip: ObjectId,
    ) -> Self {
        let relation = crate::commands::share::compare(repo, ours, tip);
        let divergence = match (relation, ours) {
            (crate::commands::share::HeadRelation::Diverged, Some(ours)) => {
                crate::commands::share::explain(repo, remote, branch, &user, ours, tip)
            }
            _ => None,
        };
        Self {
            user,
            tip: tip.to_hex(),
            relation,
            divergence,
        }
    }

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
    let Some(url) = crate::context::config_string(config, &format!("remote.{remote}.url")) else {
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
        match crate::context::config_string(config, &format!("remote.{SPACE_REMOTE}.url")) {
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
        if crate::context::config_string(config, &format!("remote.{remote}.pushurl")).as_deref()
            != Some(space.url.as_str())
        {
            remotes
                .set_url(remote, &space.url, RemoteUrlKind::Push, false)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            configured = true;
        }
        SPACE_REMOTE.to_owned()
    };
    if configure_shared(
        repo,
        &space_remote,
        &space.shared,
        space.target_branch.as_deref(),
    )? {
        configured = true;
    }
    for (name, _) in tips(repo, "refs/heads/")? {
        if is_shared(config, &name) && !space.shared.contains(&name) {
            unmark_shared(repo, &name)?;
        }
    }
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
    fetch_in(&context::discover()?, remote, mode)
}

/// Fetch `remote` into `repo`, and the other members' heads of shared
/// branches when the clone is set up for them.
pub(crate) fn fetch_in(repo: &Repository, remote: &str, mode: OutputMode) -> Result<()> {
    // Prune: a branch the server deleted (a shared branch once it's
    // integrated, say) must not linger here as a stale remote ref.
    let args = FetchArgs {
        remote: Some(remote.to_owned()),
        prune: true,
        quiet: QuietArgs { quiet: true },
        ..FetchArgs::default()
    };
    crate::commands::fetch::run_one_in(repo, &args, mode)
        .with_context(|| format!("fetching {remote}"))?;
    let config = ConfigSet::load(
        &context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    let refspec = shared_refspec(remote);
    if crate::context::config_strings(&config, &format!("remote.{remote}.fetch")).contains(&refspec)
    {
        fetch_shared_heads(repo, remote, mode)?;
    }
    Ok(())
}

/// Fetch only the other members' heads of shared branches from `remote`,
/// dropping the ones the server no longer has.
pub(crate) fn fetch_shared_heads(repo: &Repository, remote: &str, mode: OutputMode) -> Result<()> {
    let args = FetchArgs {
        remote: Some(remote.to_owned()),
        refspecs: vec![shared_refspec(remote)],
        prune: true,
        quiet: QuietArgs { quiet: true },
        ..FetchArgs::default()
    };
    crate::commands::fetch::run_one_in(repo, &args, mode)
        .with_context(|| format!("fetching shared heads from {remote}"))?;
    Ok(())
}

/// The refspec that brings other members' heads of shared branches into
/// `refs/remotes/<remote>/shared/<branch>/<user>`.
pub(crate) fn shared_refspec(remote: &str) -> String {
    format!("+{SHARED_REFS}*:refs/remotes/{remote}/shared/*")
}

/// The other members' heads of shared branch `branch` fetched from
/// `remote`: user → tip.
pub(crate) fn shared_heads(
    repo: &Repository,
    remote: &str,
    branch: &str,
) -> Result<BTreeMap<String, ObjectId>> {
    Ok(
        tips(repo, &format!("refs/remotes/{remote}/shared/{branch}/"))?
            .into_iter()
            .filter(|(user, _)| !user.contains('/'))
            .collect(),
    )
}

/// Set the clone up for shared `branches` of `remote`: fetch the other
/// members' heads, and mark each branch shared (`branch.<name>.shared`), so
/// `grit push` replaces your head without `--force`. `base` is the branch
/// they build on (`branch.<name>.sharedBase`), whose history comparisons
/// leave out. Whether config changed.
pub(crate) fn configure_shared(
    repo: &Repository,
    remote: &str,
    branches: &[String],
    base: Option<&str>,
) -> Result<bool> {
    let env = context::environment();
    let config = ConfigSet::load(
        &env,
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(&env, Some(repo.git_dir())),
    )
    .context("could not load config")?;
    let mut changed = false;
    let refspec = shared_refspec(remote);
    let fetch_key = format!("remote.{remote}.fetch");
    if !branches.is_empty()
        && !crate::context::config_strings(&config, &fetch_key).contains(&refspec)
    {
        repo.config()
            .add_value(
                ConfigScope::Local,
                &fetch_key,
                &refspec,
                &ConfigWriteOptions::new(),
            )
            .with_context(|| format!("adding {refspec} to {fetch_key}"))?;
        changed = true;
    }
    for branch in branches {
        if let Some(base) = base {
            let key = format!("branch.{branch}.sharedBase");
            if crate::context::config_string(&config, &key).as_deref() != Some(base) {
                repo.config()
                    .set(ConfigScope::Local, &key, base, &ConfigWriteOptions::new())
                    .with_context(|| format!("setting {key}"))?;
                changed = true;
            }
        }
        let key = format!("branch.{branch}.shared");
        if crate::context::config_string(&config, &key).as_deref() != Some("true") {
            repo.config()
                .set(ConfigScope::Local, &key, "true", &ConfigWriteOptions::new())
                .with_context(|| format!("setting {key}"))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Mark `branch` no longer shared here.
pub(crate) fn unmark_shared(repo: &Repository, branch: &str) -> Result<()> {
    let env = context::environment();
    let config = ConfigSet::load(
        &env,
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(&env, Some(repo.git_dir())),
    )
    .context("could not load config")?;
    let key = format!("branch.{branch}.shared");
    if crate::context::config_string(&config, &key).is_some() {
        repo.config()
            .unset(ConfigScope::Local, &key, &ConfigWriteOptions::new())
            .with_context(|| format!("unsetting {key}"))?;
    }
    Ok(())
}

/// Whether `branch` is marked shared in `config`.
pub(crate) fn is_shared(config: &ConfigSet, branch: &str) -> bool {
    crate::context::config_string(config, &format!("branch.{branch}.shared")).as_deref()
        == Some("true")
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
        grit_lib::refs::write_ref(repo.git_dir(), format!("refs/heads/{name}"), &oid)
            .with_context(|| format!("moving {name}"))
    }
}

fn set_tracking(repo: &Repository, branch: &str, remote: &str) -> Result<()> {
    for (key, value) in [
        (format!("branch.{branch}.remote"), remote.to_owned()),
        (
            format!("branch.{branch}.merge"),
            format!("refs/heads/{branch}"),
        ),
    ] {
        repo.config()
            .set(ConfigScope::Local, &key, &value, &ConfigWriteOptions::new())
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
    let specs: Vec<String> = branches
        .iter()
        .map(|b| format!("refs/heads/{b}:refs/heads/{b}"))
        .collect();
    push_specs(remote, &specs, mode)
}

/// Push refspecs `specs` to `remote`; each branch's result.
pub(crate) fn push_specs(
    remote: &str,
    specs: &[String],
    mode: OutputMode,
) -> Result<BTreeMap<String, Option<String>>> {
    if specs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut positionals = vec![remote.to_owned()];
    positionals.extend(specs.iter().cloned());
    let outcome = crate::commands::push::run(
        false,
        false,
        false,
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
            grit_lib::refs::write_ref(repo.git_dir(), format!("refs/heads/{target}"), &theirs)?;
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
    // Other members' heads of shared branches (`<remote>/shared/...`) are
    // theirs, not branches of your space.
    let remote: BTreeMap<String, ObjectId> =
        tips(&repo, &format!("refs/remotes/{}/", setup.remote))?
            .into_iter()
            .filter(|(name, _)| !name.starts_with("shared/"))
            .collect();
    let names: BTreeSet<&String> = local
        .keys()
        .chain(remote.keys())
        .filter(|n| **n != target_name && !followed.contains(n.as_str()) && n.as_str() != "HEAD")
        .collect();

    let merged = |oid: ObjectId| -> bool {
        base.is_some_and(|base| is_ancestor(&repo, oid, base).unwrap_or(false))
    };
    let shared: BTreeSet<&str> = setup.space.shared.iter().map(String::as_str).collect();
    let mut branches = Vec::new();
    let mut to_push = Vec::new();
    // Branches this sync makes in your space.
    let mut made = BTreeSet::new();
    for name in names {
        let is_shared = shared.contains(name.as_str());
        let (mut ours, theirs) = (local.get(name).copied(), remote.get(name).copied());
        let mut pulled_from = None;
        if let (true, Some(mine)) = (is_shared, ours) {
            // Another member built on your head: take theirs, the furthest
            // ahead, when every head ahead of yours is on one line.
            let ahead: Vec<(String, ObjectId)> = shared_heads(&repo, &setup.remote, name)?
                .into_iter()
                .filter(|(_, tip)| *tip != mine && is_ancestor(&repo, mine, *tip).unwrap_or(false))
                .collect();
            let furthest = ahead.iter().find(|(_, tip)| {
                ahead.iter().all(|(_, other)| {
                    other == tip || is_ancestor(&repo, *other, *tip).unwrap_or(false)
                })
            });
            if let Some((user, tip)) = furthest {
                fast_forward(
                    &repo,
                    name,
                    &format!("{}/shared/{name}/{user}", setup.remote),
                    *tip,
                )?;
                ours = Some(*tip);
                pulled_from = Some(user.clone());
            }
        }
        // A head that diverged from yours, but goes together with it
        // without losing anything: combine them, and push the result.
        let mut combined_with = Vec::new();
        if is_shared {
            for (user, tip) in shared_heads(&repo, &setup.remote, name)? {
                let Some(mine) = ours else { break };
                if crate::commands::share::compare(&repo, Some(mine), tip)
                    != crate::commands::share::HeadRelation::Diverged
                {
                    continue;
                }
                if let Some(head) = crate::commands::share::auto_combine(
                    &repo,
                    &setup.remote,
                    name,
                    &user,
                    mine,
                    tip,
                ) {
                    ours = Some(head);
                    combined_with.push(user);
                }
            }
        }
        // A branch that was shared and is gone from your space: integrated,
        // most likely. With nothing new on it, it stays here as it is (a
        // snapshot member's base doesn't hold the real commits, so `merged`
        // can't tell).
        if let (Some(mine), None, true) = (ours, theirs, self::is_shared(&config, name)) {
            let report = crate::commands::share::integrated_on(
                &repo,
                &config,
                &setup.server,
                &setup.space.project,
                name,
                mine,
            )
            .ok()
            .flatten();
            if report.is_some_and(|r| r.new == 0) {
                unmark_shared(&repo, name)?;
                continue;
            }
        }
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
                grit_lib::refs::write_ref(repo.git_dir(), format!("refs/heads/{name}"), &theirs)?;
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
            // Your head of a shared branch is yours alone: after an amend or
            // rebase here, replace it (the server keeps the old one in the
            // share's log).
            (Some(_), Some(_)) if is_shared => {
                to_push.push(name.clone());
                SyncAction::Pushed
            }
            (Some(_), Some(_)) => SyncAction::Diverged,
            (None, None) => continue,
        };
        let message = match (action, &pulled_from) {
            _ if !combined_with.is_empty() => Some(format!(
                "combined with {}'s head, which went together with yours without losing anything",
                combined_with.join("'s and ")
            )),
            (SyncAction::Diverged, _) => Some(format!(
                "{name} and {}/{name} each have commits the other lacks: switch to it and run `grit pull --rebase`, then sync again",
                setup.remote
            )),
            (_, Some(user)) => Some(format!("took {user}'s head, which builds on yours")),
            _ => None,
        };
        branches.push(SyncBranch {
            name: name.clone(),
            action,
            tip: None,
            message,
            shared: Vec::new(),
            integrated: None,
        });
        if theirs.is_none() && ours.is_some() {
            made.insert(name.clone());
        }
    }

    let pushed = push(&setup.remote, &to_push, mode)?;
    let local = tips(&repo, "refs/heads/")?;
    for branch in &mut branches {
        if let Some(Some(why)) = pushed.get(&branch.name) {
            branch.action = SyncAction::Refused;
            branch.message = Some(why.clone());
        }
        branch.tip = local.get(&branch.name).map(ObjectId::to_hex);
        if made.contains(&branch.name) && branch.action == SyncAction::Pushed {
            if let Some(tip) = local.get(&branch.name).copied() {
                branch.integrated = crate::commands::share::integrated_on(
                    &repo,
                    &config,
                    &setup.server,
                    &setup.space.project,
                    &branch.name,
                    tip,
                )
                .ok()
                .flatten();
                if branch.integrated.is_some() {
                    unmark_shared(&repo, &branch.name)?;
                }
            }
        }
        if shared.contains(branch.name.as_str()) {
            let ours = local.get(&branch.name).copied();
            for (user, tip) in shared_heads(&repo, &setup.remote, &branch.name)? {
                let head = SharedHead::new(&repo, &setup.remote, &branch.name, user, ours, tip);
                if head.relation == crate::commands::share::HeadRelation::Diverged
                    && branch.message.is_none()
                {
                    branch.message = Some(format!(
                        "{}'s head and yours have diverged: rebase onto theirs with `grit rebase {}/shared/{}/{}` (or have them rebase onto yours), then sync",
                        head.user, setup.remote, branch.name, head.user
                    ));
                }
                branch.shared.push(head);
            }
        }
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

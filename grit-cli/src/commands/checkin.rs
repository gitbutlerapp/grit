//! `grit checkin` and `grit integrate` — send a snapshot checkout's branches
//! back to the server, and merge one into the server's target branch.

use std::io::Write;

use anyhow::{bail, Result};
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::remote::{
    redact_url_for_display,
    snapshot_checkout::{
        same_branch_name, suggest_work_branch, valid_branch_name, CheckinBranch, CheckinRequest,
        CheckinRule, CheckinStatus, IntegrateRequest, IntegrateStatus, Landing,
    },
};
use grit_lib::revwalk::ReachableWalkOptions;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::checkout::{require_checkout, OpenCheckout};
use crate::commands::pull::gave_change_ids_phrase;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::TerminalProgress;

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

/// Push `branches` to their out refs on the checkout's server (the URL the
/// checkout records, whatever `origin` is now) and ask it to graft them.
/// `gave_change_ids` is how many commits were given a change id first.
fn check_in(
    co: &OpenCheckout,
    branches: &[(String, ObjectId)],
    force: bool,
    gave_change_ids: usize,
    mode: OutputMode,
) -> Result<CheckinOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = TerminalProgress::new(mode);
    let pushed = co
        .repo
        .snapshot_checkouts()
        .push_for_checkin(
            &co.marker,
            branches,
            &mut progress,
            &crate::interrupt::process_cancel_token(),
            Some(&crate::context::http_client_factory()),
        )
        .map_err(|e| match e {
            remote @ grit_lib::error::Error::Remote(_) => {
                crate::commands::fetch::map_remote_error(remote)
            }
            other => other.into(),
        })?;
    drop(progress);
    if let Some(bad) = pushed.results.iter().find(|r| r.status.is_error()) {
        bail!(
            "the server refused {}: {}",
            bad.remote_ref,
            bad.message.as_deref().unwrap_or("rejected")
        );
    }
    let request = CheckinRequest {
        branches: branches
            .iter()
            .map(|(name, tip)| CheckinBranch {
                name: name.clone(),
                raw: tip.to_hex(),
            })
            .collect(),
        force,
    };
    let response = co.client()?.checkin(&co.marker.id, &request)?;
    let branches: Vec<CheckinBranchJson> = response
        .results
        .into_iter()
        .map(|r| CheckinBranchJson {
            name: r.name,
            status: match r.status {
                CheckinStatus::Created => "created",
                CheckinStatus::Updated => "updated",
                CheckinStatus::Unchanged => "unchanged",
                CheckinStatus::Refused => "refused",
            }
            .to_owned(),
            grafted: r.grafted,
            previous: r.previous,
            commits: r.commits,
            rule: r.rule.map(CheckinRuleJson::from),
            message: r.message,
            reason: r.reason.map(|reason| reason.kind().to_owned()),
        })
        .collect();
    Ok(CheckinOutcome {
        url: redact_url_for_display(&co.marker.url),
        gave_change_ids,
        refused: branches.iter().any(|b| b.status == "refused"),
        branches,
    })
}

/// The local branches named, or every branch with work on the snapshot
/// (possibly none).
fn chosen_branches(co: &OpenCheckout, names: &[String]) -> Result<Vec<(String, ObjectId)>> {
    if names.is_empty() {
        return Ok(co.repo.snapshot_checkouts().work_branches(&co.marker)?);
    }
    names
        .iter()
        .map(|name| {
            let tip = grit_lib::refs::resolve_ref(co.repo.git_dir(), format!("refs/heads/{name}"))
                .map_err(|_| anyhow::anyhow!("there is no branch {name} here"))?;
            Ok((name.clone(), tip))
        })
        .collect()
}

/// `grit checkin [<branch>...] [--force]`: commits made with plain Git on
/// those branches (all of them when none is named) are given a change id
/// first. With no branch named and none with work on the snapshot, there is
/// nothing to do, and that is not an error.
pub fn run(names: &[String], force: bool, mode: OutputMode) -> Result<CheckinOutcome> {
    let co = require_checkout()?;
    let gave = co.repo.snapshot_checkouts().give_change_ids(
        &co.marker,
        (!names.is_empty()).then_some(names),
        co.repo.identity_now(),
    )?;
    let branches = chosen_branches(&co, names)?;
    if branches.is_empty() {
        return Ok(CheckinOutcome {
            url: redact_url_for_display(&co.marker.url),
            gave_change_ids: gave,
            branches: Vec::new(),
            refused: false,
        });
    }
    check_in(&co, &branches, force, gave, mode)
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
    pub local: Option<crate::commands::sync::SyncTarget>,
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
                // The refresh worked; moving to a new work branch did not.
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

/// What `grit integrate` did in the checkout after the server merged.
#[derive(Default)]
struct FollowUp {
    refresh: Option<crate::commands::pull::PullOutcome>,
    moved_to: Option<String>,
    error: Option<String>,
}

/// `err` in words, naming the files when uncommitted changes are in the way.
fn failure_text(err: &anyhow::Error) -> String {
    crate::error_map::integrate_failure_text(err)
}

/// Bring the checkout onto the target the merge made, in the order
/// [`land_integration`](grit_lib::repo_handles::RepositorySnapshotCheckouts::land_integration) asks for: refresh as `grit pull` does, then move off
/// the integrated branch when the library says so ([`Landing::move_off`]).
/// The merge stands whatever happens here, so failures are reported, not
/// returned.
fn follow_up(co: &OpenCheckout, name: &str, landing: &Landing, mode: OutputMode) -> FollowUp {
    let refresh =
        match crate::commands::pull::run(&crate::commands::pull::PullRunArgs::refresh(), mode) {
            Ok(refresh) => refresh,
            Err(e) => {
                return FollowUp {
                    error: Some(failure_text(&e)),
                    ..FollowUp::default()
                }
            }
        };
    let mut done = FollowUp {
        refresh: Some(refresh),
        ..FollowUp::default()
    };
    let wanted = suggest_work_branch(
        crate::context::config_string(&co.config, "user.email").as_deref(),
        crate::commands::checkout::random_pair(),
    );
    let moved = crate::context::discover()
        .and_then(|repo| Ok(landing.move_off(&repo, &co.client()?, &wanted)?));
    match moved {
        Ok(moved) => done.moved_to = moved.map(|m| m.to),
        Err(e) => {
            done.error = Some(format!(
                "moving off {name} to a new work branch failed: {}",
                failure_text(&e)
            ))
        }
    }
    done
}

/// `grit integrate <branch>`: check the branch in (when it is here, after
/// giving its commits made with plain Git a change id), merge it into the
/// target on the server, then refresh this checkout. When the merge
/// went into the branch this checkout follows, the branch's commits are in
/// its next snapshot, so they are never replayed: when `HEAD` is on the
/// branch it moves to the new snapshot, keeping uncommitted changes, and onto
/// a fresh work branch ([`land_integration`](grit_lib::repo_handles::RepositorySnapshotCheckouts::land_integration) decides).
pub fn integrate(name: &str, without: &[String], mode: OutputMode) -> Result<IntegrateOutcome> {
    if crate::commands::checkout::open_checkout_here()?.is_none() {
        return integrate_hosted(name, without, mode);
    }
    if !without.is_empty() {
        bail!("--without is for shared branches, in a clone of a project on a grit hosting server");
    }
    let co = require_checkout()?;
    if same_branch_name(name, &co.marker.branch) {
        bail!(
            "{name} is the server's target branch; name the branch to merge into it, such as your work branch (`grit branch` lists them)"
        );
    }
    if !valid_branch_name(name) {
        bail!("{name} is not a valid branch name");
    }
    let gave = co.repo.snapshot_checkouts().give_change_ids(
        &co.marker,
        Some(&[name.to_owned()]),
        co.repo.identity_now(),
    )?;
    let local = co
        .repo
        .snapshot_checkouts()
        .work_branches(&co.marker)?
        .into_iter()
        .find(|(n, _)| n == name);
    let (checkin, checked_in) = match local {
        Some(branch) => {
            let tip = branch.1;
            let done = check_in(&co, &[branch], false, gave, mode)?;
            if done.refused {
                let why = done
                    .branches
                    .first()
                    .and_then(|b| b.message.clone())
                    .unwrap_or_default();
                bail!("{name} could not be checked in: {why}");
            }
            (Some(done), Some(tip))
        }
        None => (None, None),
    };
    let response = co.client()?.integrate(&IntegrateRequest {
        name: name.to_owned(),
        author: co.author(),
    })?;
    let (follow, follows) = match co
        .repo
        .snapshot_checkouts()
        .land_integration(&co.marker, &response, checked_in)
    {
        Ok(landing) => (
            follow_up(&co, name, &landing, mode),
            matches!(landing, Landing::Elsewhere { .. }).then(|| co.marker.branch.clone()),
        ),
        Err(e) => (
            FollowUp {
                error: Some(failure_text(&e.into())),
                ..FollowUp::default()
            },
            None,
        ),
    };
    Ok(IntegrateOutcome {
        name: response.name,
        target: response.target,
        status: match response.status {
            IntegrateStatus::Merged => "merged",
            IntegrateStatus::AlreadyIntegrated => "already_integrated",
        }
        .to_owned(),
        previous: response.previous,
        head: response.head,
        commits: response.commits,
        checkin,
        refresh: follow.refresh,
        moved_to: follow.moved_to,
        follows,
        refresh_error: follow.error,
        project: None,
        pull: None,
        url: None,
        local: None,
        unshared: Vec::new(),
    })
}

/// `grit integrate <branch>` in a clone of a project on a grit hosting
/// server: push the branch to your space, have the server merge it into the
/// target (the organization's main, through a pull request, for a copy; the
/// project's own main otherwise), then bring the target here up to date.
fn integrate_hosted(name: &str, without: &[String], mode: OutputMode) -> Result<IntegrateOutcome> {
    use crate::commands::sync::{ensure_space, fetch, push, sync_target, tips};
    use grit_lib::config::ConfigSet;
    use grit_lib::remote::grit_server_space::{
        integrate_branch, IntegrateBranchRequest, IntegrateBranchStatus,
    };
    use grit_lib::remote::HttpClientFactory as _;

    let repo = crate::context::discover()?;
    let config = ConfigSet::load(
        &crate::context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &crate::context::environment(),
            Some(repo.git_dir()),
        ),
    )?;
    let Some(setup) = ensure_space(&repo, &config, "origin")? else {
        bail!(
            "grit integrate works in a snapshot checkout (`grit checkout`), or in a clone of a project on a grit hosting server such as grithub"
        );
    };
    let target = setup
        .space
        .target_branch
        .clone()
        .unwrap_or_else(|| "main".to_owned());
    if same_branch_name(name, &target) || setup.space.followed.iter().any(|f| f == name) {
        bail!("{name} is the target branch; name the branch to merge into it (`grit branch` lists them)");
    }
    let Some(tip) = tips(&repo, "refs/heads/")?.get(name).copied() else {
        bail!("there is no branch {name} here");
    };
    if let Some(Some(why)) = push(&setup.remote, &[name.to_owned()], mode)?.get(name) {
        bail!(
            "{name} could not be pushed to {}: {why}; run `grit sync`, then integrate again",
            setup.space.project
        );
    }
    let http = crate::context::http_client_factory().create(&config)?;
    let reply = integrate_branch(
        http.as_ref(),
        &setup.server,
        &setup.space.project,
        &IntegrateBranchRequest {
            branch: name.to_owned(),
            without: without.to_vec(),
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let status = match reply.status {
        IntegrateBranchStatus::Merged => "merged",
        IntegrateBranchStatus::AlreadyIntegrated => "already_integrated",
        IntegrateBranchStatus::Started => "started",
        IntegrateBranchStatus::Refused => {
            let why = reply
                .message
                .unwrap_or_else(|| "the server refused".to_owned());
            match reply.url {
                Some(url) => bail!("{why}\n  {url}"),
                None => bail!("{why}"),
            }
        }
    };
    if !reply.unshared.is_empty() {
        crate::commands::sync::unmark_shared(&repo, name)?;
    }
    fetch("origin", mode)?;
    let local = sync_target(&repo, "origin", &reply.target_branch)?;
    let commits = match reply
        .previous
        .as_deref()
        .and_then(|p| ObjectId::from_hex(p).ok())
    {
        Some(previous) => repo
            .revwalk()
            .reachable_excluding(
                &[tip],
                &[previous],
                &ReachableWalkOptions::default().with_limit(
                    grit_lib::revwalk::merge_base::ReachableWalkLimit::CountAllRetainNewest(0),
                ),
            )
            .map(|(count, _)| count)
            .unwrap_or(0),
        None => 0,
    };
    Ok(IntegrateOutcome {
        name: name.to_owned(),
        target: reply.target_branch,
        status: status.to_owned(),
        previous: reply.previous.unwrap_or_default(),
        head: reply.head.unwrap_or_default(),
        commits,
        checkin: None,
        refresh: None,
        moved_to: None,
        follows: None,
        refresh_error: None,
        project: Some(reply.project),
        pull: reply.pull,
        url: reply.url,
        local,
        unshared: reply.unshared,
    })
}

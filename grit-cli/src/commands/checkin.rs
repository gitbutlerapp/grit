//! `grit checkin` and `grit integrate` — send a snapshot checkout's branches
//! back to the server, and merge one into the server's target branch.

use std::io::Write;

use anyhow::{bail, Result};
use grit_lib::error::Error as LibError;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::pull::PullError;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::snapshot_checkout::{
    branches_with_work, land_integration, push_for_checkin, same_branch_name, suggest_work_branch,
    valid_branch_name, CheckinBranch, CheckinRequest, CheckinStatus, CheckoutError,
    IntegrateRequest, IntegrateStatus, Landing,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::checkout::{require_checkout, OpenCheckout};
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::TerminalProgress;

/// One branch of `grit checkin`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckinBranchJson {
    /// The branch.
    pub name: String,
    /// `created`, `updated`, `unchanged` or `refused`.
    pub status: String,
    /// The commit on the server's history the branch became.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grafted: Option<String>,
    /// Where the server's branch was before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Commits replayed onto the server's history.
    pub commits: usize,
    /// Why it was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// What kind of refusal it was: `invalid_name`, `target_branch`,
    /// `case_conflict`, `invalid_tip`, `not_pushed`, `not_on_snapshot`,
    /// `conflict`, `merge_conflict` or `diverged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Result of `grit checkin`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckinOutcome {
    /// The server repository.
    pub url: String,
    /// One entry per branch; empty when no branch had commits to check in.
    pub branches: Vec<CheckinBranchJson>,
    /// Whether any branch was refused (the exit status is then `1`).
    pub refused: bool,
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// What `grit checkin` says when no branch has work on the snapshot.
const NOTHING_TO_CHECK_IN: &str =
    "nothing to check in: no branch has commits on top of the snapshot";

impl HumanRender for CheckinOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.branches.is_empty() {
            outln!(w, "· {NOTHING_TO_CHECK_IN}");
        }
        for b in &self.branches {
            let at = b.grafted.as_deref().map(short).unwrap_or_default();
            let commits = if b.commits == 1 {
                "1 commit".to_owned()
            } else {
                format!("{} commits", b.commits)
            };
            match b.status.as_str() {
                "created" => outln!(w, "✓ {} created on the server at {at} ({commits})", b.name),
                "updated" => outln!(
                    w,
                    "✓ {} updated on the server {} → {at} ({commits})",
                    b.name,
                    b.previous.as_deref().map(short).unwrap_or_default()
                ),
                "unchanged" => outln!(w, "· {} already at {at} on the server", b.name),
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
        if self.branches.is_empty() {
            outln!(w, "- {NOTHING_TO_CHECK_IN}");
            return Ok(());
        }
        outln!(w, "| Branch | Status | Server commit | Commits |");
        outln!(w, "| --- | --- | --- | ---: |");
        for b in &self.branches {
            outln!(
                w,
                "| `{}` | {} | {} | {} |",
                b.name,
                b.status,
                b.grafted
                    .as_deref()
                    .map(|g| format!("`{g}`"))
                    .unwrap_or_default(),
                b.commits
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
fn check_in(
    co: &OpenCheckout,
    branches: &[(String, ObjectId)],
    force: bool,
    mode: OutputMode,
) -> Result<CheckinOutcome> {
    let mut progress = TerminalProgress::new(mode);
    let pushed = push_for_checkin(
        &co.repo,
        &co.marker,
        branches,
        &mut progress,
        &NeverCancel,
        Some(&crate::context::http_client_factory()),
    )
    .map_err(|e| match e {
        CheckoutError::Remote(remote) => crate::commands::fetch::map_remote_error(*remote),
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
            message: r.message,
            reason: r.reason.map(|reason| reason.kind().to_owned()),
        })
        .collect();
    Ok(CheckinOutcome {
        url: co.marker.url.clone(),
        refused: branches.iter().any(|b| b.status == "refused"),
        branches,
    })
}

/// The local branches named, or every branch with work on the snapshot
/// (possibly none).
fn chosen_branches(co: &OpenCheckout, names: &[String]) -> Result<Vec<(String, ObjectId)>> {
    if names.is_empty() {
        return Ok(branches_with_work(&co.repo, &co.marker)?);
    }
    names
        .iter()
        .map(|name| {
            let tip = grit_lib::refs::resolve_ref(co.repo.git_dir(), &format!("refs/heads/{name}"))
                .map_err(|_| anyhow::anyhow!("there is no branch {name} here"))?;
            Ok((name.clone(), tip))
        })
        .collect()
}

/// `grit checkin [<branch>...] [--force]`. With no branch named and none with
/// work on the snapshot, there is nothing to do, and that is not an error.
pub fn run(names: &[String], force: bool, mode: OutputMode) -> Result<CheckinOutcome> {
    let co = require_checkout()?;
    let branches = chosen_branches(&co, names)?;
    if branches.is_empty() {
        return Ok(CheckinOutcome {
            url: co.marker.url.clone(),
            branches: Vec::new(),
            refused: false,
        });
    }
    check_in(&co, &branches, force, mode)
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
}

impl IntegrateOutcome {
    /// Whether the checkout needs attention after the merge: its refresh
    /// failed, or stopped on a conflict or a refusal (the exit status is 1).
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.refresh_error.is_some()
            || self
                .refresh
                .as_ref()
                .is_some_and(|r| crate::commands::pull::pull_check_exit_code(r) != 0)
    }
}

impl HumanRender for IntegrateOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(c) = &self.checkin {
            c.render_human(w, ctx)?;
        }
        match self.status.as_str() {
            "merged" => outln!(
                w,
                "✓ Merged {} into {} on the server: {} → {} ({} commit{})",
                self.name,
                self.target,
                short(&self.previous),
                short(&self.head),
                self.commits,
                if self.commits == 1 { "" } else { "s" }
            ),
            _ => outln!(
                w,
                "· {} already has every commit of {}",
                self.target,
                self.name
            ),
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
    let text = format!("{err:#}");
    let paths = err.chain().find_map(|cause| {
        let lib = match cause.downcast_ref::<PullError>() {
            Some(PullError::Repository(lib)) => Some(lib),
            _ => cause.downcast_ref::<LibError>(),
        };
        match lib {
            Some(LibError::WouldOverwrite { paths, .. }) if !paths.is_empty() => {
                Some(paths.join(", "))
            }
            _ => None,
        }
    });
    match paths {
        Some(paths) => format!("{text}: {paths}"),
        None => text,
    }
}

/// Bring the checkout onto the target the merge made, in the order
/// [`land_integration`] asks for: refresh as `grit pull` does, then move off
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
        co.config.get("user.email").as_deref(),
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

/// `grit integrate <branch>`: check the branch in (when it is here), merge it
/// into the target on the server, then refresh this checkout. When the merge
/// went into the branch this checkout follows, the branch's commits are in
/// its next snapshot, so they are never replayed: when `HEAD` is on the
/// branch it moves to the new snapshot, keeping uncommitted changes, and onto
/// a fresh work branch ([`land_integration`] decides).
pub fn integrate(name: &str, mode: OutputMode) -> Result<IntegrateOutcome> {
    let co = require_checkout()?;
    if same_branch_name(name, &co.marker.branch) {
        bail!(
            "{name} is the server's target branch; name the branch to merge into it, such as your work branch (`grit branch` lists them)"
        );
    }
    if !valid_branch_name(name) {
        bail!("{name} is not a valid branch name");
    }
    let local = branches_with_work(&co.repo, &co.marker)?
        .into_iter()
        .find(|(n, _)| n == name);
    let (checkin, checked_in) = match local {
        Some(branch) => {
            let tip = branch.1;
            let done = check_in(&co, &[branch], false, mode)?;
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
    let (follow, follows) = match land_integration(&co.repo, &co.marker, &response, checked_in) {
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
    })
}

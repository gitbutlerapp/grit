//! `grit checkin` and `grit integrate` — send a snapshot checkout's branches
//! back to the server, and merge one into the server's target branch.

use anyhow::{bail, Result};
use grit_lib::objects::ObjectId;
use grit_lib::remote::{
    redact_url_for_display,
    snapshot_checkout::{
        same_branch_name, suggest_work_branch, valid_branch_name, CheckinBranch, CheckinRequest,
        IntegrateRequest, IntegrateStatus, Landing,
    },
};
use grit_lib::revwalk::ReachableWalkOptions;

use crate::commands::checkout::{require_checkout, OpenCheckout};
use crate::dispatch::CommandInteraction;
use crate::progress::TerminalProgress;

pub use crate::render::checkin::*;

/// Push `branches` to their out refs on the checkout's server (the URL the
/// checkout records, whatever `origin` is now) and ask it to graft them.
/// `gave_change_ids` is how many commits were given a change id first.
fn check_in(
    co: &OpenCheckout,
    branches: &[(String, ObjectId)],
    force: bool,
    gave_change_ids: usize,
    interaction: CommandInteraction,
) -> Result<CheckinOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = TerminalProgress::new(interaction.human_progress);
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
    let refused = response.any_refused();
    let branches: Vec<CheckinBranchJson> = response
        .results
        .into_iter()
        .map(|r| CheckinBranchJson {
            name: r.name,
            status: r.status.as_str().to_owned(),
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
        refused,
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
            let tip = co
                .repo
                .references()
                .resolve(format!("refs/heads/{name}"))
                .map_err(|_| anyhow::anyhow!("there is no branch {name} here"))?;
            Ok((name.clone(), tip))
        })
        .collect()
}

/// `grit checkin [<branch>...] [--force]`: commits made with plain Git on
/// those branches (all of them when none is named) are given a change id
/// first. With no branch named and none with work on the snapshot, there is
/// nothing to do, and that is not an error.
pub fn run(
    names: &[String],
    force: bool,
    interaction: CommandInteraction,
) -> Result<CheckinOutcome> {
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
    check_in(&co, &branches, force, gave, interaction)
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
fn follow_up(
    co: &OpenCheckout,
    name: &str,
    landing: &Landing,
    interaction: CommandInteraction,
) -> FollowUp {
    let refresh = match crate::commands::pull::run(
        &crate::commands::pull::PullRunArgs::refresh(),
        interaction.human_progress,
    ) {
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
pub fn integrate(
    name: &str,
    without: &[String],
    interaction: CommandInteraction,
) -> Result<IntegrateOutcome> {
    if crate::commands::checkout::open_checkout_here()?.is_none() {
        return integrate_hosted(name, without, interaction);
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
            let done = check_in(&co, &[branch], false, gave, interaction)?;
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
            follow_up(&co, name, &landing, interaction),
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
fn integrate_hosted(
    name: &str,
    without: &[String],
    interaction: CommandInteraction,
) -> Result<IntegrateOutcome> {
    let human_progress = interaction.human_progress;
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
    if let Some(Some(why)) = push(&setup.remote, &[name.to_owned()], human_progress)?.get(name) {
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
    fetch("origin", human_progress)?;
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

mod dispatch;
pub use dispatch::{dispatch, dispatch_integrate};

//! Integrated-share detection and `grit share resolve` for pushed-again branches.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::remote::grit_server_auth::ServerUrl;
use grit_lib::remote::grit_server_share::{ShareRequest, ShareState};
use grit_lib::repo::Repository;

use crate::commands::sync::{configure_shared, fetch, tips};
use crate::dispatch::CommandInteraction;
use crate::render::share::ShareResolveOutcome;
use crate::render::shared_branch::{IntegratedJson, ResolveOptionJson};

use super::divergence::move_branch;
use super::ops::{ask_about, Here};

pub(crate) const INTEGRATED_OPTIONS: [&str; 3] = ["rebase", "rename", "reshare"];

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

pub(crate) fn integrated_report(
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
pub(crate) fn resolve_integrated(
    here: &Here,
    branch: &str,
    ours: ObjectId,
    report: IntegratedJson,
    option: Option<&str>,
    name: Option<&str>,
    interaction: CommandInteraction,
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
            fetch("origin", interaction.human_progress)?;
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
                _ => bail!("rebasing {branch} onto {target} returned a result this version of grit cannot handle"),
            };
            if head != ours {
                move_branch(&here.repo, branch, head)?;
            }
            outcome.head = Some(head.to_hex());
            let spec = format!("+refs/heads/{branch}:refs/heads/{branch}");
            let pushed = crate::commands::sync::push_specs(
                &here.remote,
                &[spec],
                interaction.human_progress,
            )?;
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
            here.repo
                .branches()
                .rename(&grit_lib::porcelain::branch::BranchRenameOptions::new(
                    branch.to_owned(),
                    new.to_owned(),
                ))
                .with_context(|| format!("renaming {branch} to {new}"))?;
            let specs = vec![
                format!("refs/heads/{new}:refs/heads/{new}"),
                format!(":refs/heads/{branch}"),
            ];
            let pushed = crate::commands::sync::push_specs(
                &here.remote,
                &specs,
                interaction.human_progress,
            )?;
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

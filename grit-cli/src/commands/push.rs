//! `grit push` — publish refs to a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::push_plan::{
    configured_remote_for_push, resolve_push, PushPlanContext, PushPlanError, PushPlanKind,
    PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::PushOptions;
use grit_lib::remote::{DefaultHttpClientFactory, ListRefsOptions, Remote};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;

use crate::commands::auth;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use grit_lib::remote::fetch::NoProgress;

/// Result of `grit push`: the per-ref outcomes for the remote.
#[derive(Serialize, JsonSchema)]
pub struct PushOutcome {
    pub remote: String,
    /// High-level push kind (`branch`, `tags`, `delete`, `refspecs`).
    pub kind: PushKind,
    /// Local branch short name when [`Self::kind`] is `branch` (legacy `branch` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub results: Vec<PushRefResult>,
    pub rejected: bool,
    /// True when `--dry-run` was used (no refs or objects written).
    pub dry_run: bool,
}

/// JSON classification for [`PushOutcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    Branch,
    Tags,
    Delete,
    Refspecs,
}

impl From<PushPlanKind> for PushKind {
    fn from(k: PushPlanKind) -> Self {
        match k {
            PushPlanKind::Branch => Self::Branch,
            PushPlanKind::Tags => Self::Tags,
            PushPlanKind::Delete => Self::Delete,
            PushPlanKind::Refspecs => Self::Refspecs,
        }
    }
}

/// Per-ref push status in JSON (`ok`, `up_to_date`, or `rejected`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushRefOutcomeStatus {
    Ok,
    UpToDate,
    Rejected,
}

impl From<&PushRefStatus> for PushRefOutcomeStatus {
    fn from(status: &PushRefStatus) -> Self {
        match status {
            PushRefStatus::Ok => Self::Ok,
            PushRefStatus::UpToDate => Self::UpToDate,
            _ => Self::Rejected,
        }
    }
}

/// One ref's push result.
#[derive(Serialize, JsonSchema)]
pub struct PushRefResult {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub status: PushRefOutcomeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_oid: Option<String>,
    pub forced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

impl HumanRender for PushOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let label = match (self.branch.as_deref(), self.kind) {
            (Some(b), _) => b,
            (_, PushKind::Tags) => "--tags",
            (_, PushKind::Delete) => "--delete",
            _ => "refs",
        };
        for result in &self.results {
            let target = format!("{} {}", self.remote, result.ref_name);
            match result.status {
                PushRefOutcomeStatus::Ok => {
                    if self.dry_run {
                        outln!(w, "  would push {label} → {target}");
                    } else {
                        outln!(w, "  pushed {label} → {target}");
                    }
                }
                PushRefOutcomeStatus::UpToDate => outln!(w, "  {target} already up to date"),
                PushRefOutcomeStatus::Rejected => outln!(
                    w,
                    "  rejected {target}: {}",
                    result.reason.as_deref().unwrap_or("rejected")
                ),
            }
        }
        Ok(())
    }
}

impl MarkdownRender for PushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let label = self.branch.as_deref().unwrap_or("refs");
        markdown::print_push_results(w, &self.remote, label, self.dry_run, &self.results)?;
        Ok(())
    }
}

/// Positional `[<remote>] [<refspec>...]` plus flags.
pub fn run(
    tags: bool,
    delete_refs: &[String],
    remote_flag: Option<String>,
    positionals: &[String],
    dry_run: bool,
) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;

    let (remote_override, refspecs) = split_positionals(&config, &repo, remote_flag, positionals)?;

    let plan_ctx = load_remote_ref_names(&repo, &config, remote_override.as_deref())?;

    let plan = resolve_push(
        &repo,
        &config,
        &PushRequest {
            remote: remote_override,
            refspecs,
            tags,
            delete: delete_refs.to_vec(),
        },
        &plan_ctx,
    )
    .map_err(plan_error)?;

    let factory = DefaultHttpClientFactory;
    let report = match push_once(&repo, &config, &plan.remote, &plan.specs, dry_run, &factory) {
        Ok(report) => report,
        Err(err) => {
            let url = config
                .get(&format!("remote.{}.url", plan.remote))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                push_once(&repo, &config, &plan.remote, &plan.specs, dry_run, &factory)?
            } else {
                return Err(err);
            }
        }
    };

    let mut rejected = false;
    let results = report
        .results
        .iter()
        .map(|result| {
            let status = PushRefOutcomeStatus::from(&result.status);
            let reason = match result.status {
                PushRefStatus::Ok | PushRefStatus::UpToDate => None,
                PushRefStatus::RejectNonFastForward => {
                    rejected = true;
                    Some("not a fast-forward — run `grit pull` first".to_owned())
                }
                _ => {
                    rejected = true;
                    Some(
                        result
                            .message
                            .clone()
                            .unwrap_or_else(|| "rejected".to_owned()),
                    )
                }
            };
            PushRefResult {
                ref_name: result.remote_ref.clone(),
                status,
                reason,
                old_oid: result.old_oid.map(|o| o.to_hex()),
                new_oid: result.new_oid.map(|o| o.to_hex()),
                forced: result.forced,
                src: result.local_ref.clone(),
            }
        })
        .collect();

    Ok(PushOutcome {
        remote: plan.remote,
        kind: plan.kind.into(),
        branch: plan.branch_label,
        results,
        rejected,
        dry_run,
    })
}

fn plan_error(err: PushPlanError) -> anyhow::Error {
    anyhow::Error::msg(err.to_string())
}

fn split_positionals(
    config: &ConfigSet,
    repo: &grit_lib::repo::Repository,
    remote_flag: Option<String>,
    positionals: &[String],
) -> Result<(Option<String>, Vec<String>)> {
    if let Some(remote) = remote_flag {
        return Ok((Some(remote), positionals.to_vec()));
    }
    if positionals.is_empty() {
        return Ok((None, Vec::new()));
    }
    if is_configured_remote(config, &positionals[0]) {
        let remote = positionals[0].clone();
        let specs = positionals[1..].to_vec();
        return Ok((Some(remote), specs));
    }
    if positionals.len() == 1 && looks_like_remote_name(&positionals[0]) {
        // Single token that is not a configured remote: treat as refspec.
        return Ok((None, positionals.to_vec()));
    }
    if positionals.len() > 1
        && !is_configured_remote(config, &positionals[0])
        && !looks_like_remote_name(&positionals[0])
    {
        return Err(plan_error(PushPlanError::UnknownRemote(
            positionals[0].clone(),
        )));
    }
    let _ = repo;
    Ok((None, positionals.to_vec()))
}

fn is_configured_remote(config: &ConfigSet, name: &str) -> bool {
    config
        .get(&format!("remote.{name}.url"))
        .filter(|u| !u.trim().is_empty())
        .is_some()
}

fn looks_like_remote_name(name: &str) -> bool {
    name.contains("://") || name.ends_with(".git") || name.starts_with('/')
}

fn load_remote_ref_names(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_override: Option<&str>,
) -> Result<PushPlanContext> {
    let remote = configured_remote_for_push(repo, config, remote_override).map_err(plan_error)?;
    let Ok(remote_handle) = Remote::from_config(config, &remote) else {
        return Ok(PushPlanContext::default());
    };
    let factory = DefaultHttpClientFactory;
    let advertised = remote_handle
        .list_refs(Some(repo), &ListRefsOptions::default(), Some(&factory))
        .unwrap_or_default();
    let remote_ref_names: HashSet<String> = advertised
        .into_iter()
        .map(|r| r.name)
        .filter(|n| !n.ends_with("^{}"))
        .collect();
    Ok(PushPlanContext { remote_ref_names })
}

fn push_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    specs: &[grit_lib::remote::transfer::PushRefSpec],
    dry_run: bool,
    factory: &DefaultHttpClientFactory,
) -> Result<grit_lib::remote::transfer::PushOutcome> {
    Remote::from_config(config, remote_name)
        .map_err(|e| anyhow::Error::msg(e.to_string()))?
        .push(
            repo,
            specs,
            PushOptions {
                dry_run,
                tracking_remote: Some(remote_name.to_owned()),
                ..Default::default()
            },
            &mut NoProgress,
            Some(factory),
        )
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_status_serializes_snake_case() {
        for (status, expected) in [
            (PushRefOutcomeStatus::Ok, "ok"),
            (PushRefOutcomeStatus::UpToDate, "up_to_date"),
            (PushRefOutcomeStatus::Rejected, "rejected"),
        ] {
            let result = PushRefResult {
                ref_name: "refs/heads/main".to_owned(),
                status,
                reason: None,
                old_oid: None,
                new_oid: None,
                forced: false,
                src: None,
            };
            let value = serde_json::to_value(&result).expect("json");
            assert_eq!(value["status"].as_str(), Some(expected));
        }
    }
}

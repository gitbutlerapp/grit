//! `grit push` — publish refs to a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::remote::push_plan::{
    apply_set_upstream_after_push, load_push_plan_context, open_push_destination, parse_force_mode,
    push_follow_tags_config, resolve_push, resolve_push_invocation, PushPlanError, PushPlanKind,
    PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::PushOptions;
use grit_lib::remote::DefaultHttpClientFactory;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::TerminalProgress;
use crate::quiet_args::QuietArgs;
use grit_lib::progress::NeverCancel;

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
    /// Branch short names whose upstream was configured (`-u` / `--set-upstream`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream_set: Option<Vec<String>>,
    /// Hook and diagnostic lines from the remote (`remote: …` in human output).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub remote_messages: Vec<String>,
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
                PushRefOutcomeStatus::Rejected => {
                    let detail = result.reason.as_deref().unwrap_or("rejected");
                    if detail == "stale" {
                        outln!(
                            w,
                            "  rejected {target}: stale remote ref — run `grit fetch` and try again"
                        );
                    } else if detail == "remote_ref_missing" {
                        outln!(w, "  rejected {target}: remote ref does not exist");
                    } else {
                        outln!(w, "  rejected {target}: {detail}");
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for PushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let label = self.branch.as_deref().unwrap_or("refs");
        markdown::print_push_remote_messages(w, &self.remote_messages)?;
        markdown::print_push_results(w, &self.remote, label, self.dry_run, &self.results)?;
        Ok(())
    }
}

/// Wire and local options forwarded to [`PushOptions`].
struct PushWireOptions<'a> {
    dry_run: bool,
    atomic: bool,
    push_options: &'a [String],
    remote_ref_tips: Option<Vec<ObjectId>>,
}

/// Positional `[<remote>] [<refspec>...]` plus flags.
#[allow(clippy::too_many_arguments)]
pub fn run(
    tags: bool,
    delete_refs: &[String],
    remote_flag: Option<String>,
    positionals: &[String],
    dry_run: bool,
    force: bool,
    force_with_lease: Option<Option<String>>,
    set_upstream: bool,
    atomic: bool,
    push_options: &[String],
    follow_tags: bool,
    quiet: &QuietArgs,
    mode: OutputMode,
) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;

    let invocation = resolve_push_invocation(&config, remote_flag.as_deref(), positionals)
        .map_err(plan_error)?;

    let factory = DefaultHttpClientFactory;
    let plan_ctx =
        load_push_plan_context(&repo, &config, invocation.remote.as_deref(), Some(&factory))
            .map_err(plan_error)?;

    let force_mode = parse_force_mode(&repo, force, force_with_lease).map_err(plan_error)?;

    let follow = follow_tags || push_follow_tags_config(&config);

    let push_request = PushRequest {
        remote: invocation.remote,
        refspecs: invocation.refspecs,
        tags,
        delete: delete_refs.to_vec(),
        force: force_mode,
        follow_tags: follow,
        set_upstream,
    };

    let plan = resolve_push(&repo, &config, &push_request, &plan_ctx).map_err(plan_error)?;

    let wire = PushWireOptions {
        dry_run,
        atomic,
        push_options,
        remote_ref_tips: plan_ctx.remote_tips,
    };
    let mut progress = quiet.progress(mode);
    let report = match push_once(
        &repo,
        &config,
        &plan.remote,
        &plan.specs,
        &wire,
        &factory,
        &mut progress,
    ) {
        Ok(report) => report,
        Err(err) => {
            let url = config
                .get(&format!("remote.{}.url", plan.remote))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                push_once(
                    &repo,
                    &config,
                    &plan.remote,
                    &plan.specs,
                    &wire,
                    &factory,
                    &mut progress,
                )?
            } else {
                return Err(err);
            }
        }
    };

    let upstream_set = apply_set_upstream_after_push(
        &repo,
        &plan.remote,
        push_request.set_upstream,
        dry_run,
        &plan.specs,
        &report.results,
    )
    .map_err(|e| anyhow::Error::msg(e.to_string()))?;

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
                PushRefStatus::RejectStale => {
                    rejected = true;
                    Some("stale".to_owned())
                }
                PushRefStatus::RejectRemoteRefMissing => {
                    rejected = true;
                    Some("remote_ref_missing".to_owned())
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
        upstream_set: (!upstream_set.is_empty()).then_some(upstream_set),
        remote_messages: report.remote_messages,
    })
}

fn plan_error(err: PushPlanError) -> anyhow::Error {
    anyhow::Error::new(err)
}

fn push_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    specs: &[grit_lib::remote::transfer::PushRefSpec],
    wire: &PushWireOptions<'_>,
    factory: &DefaultHttpClientFactory,
    progress: &mut TerminalProgress,
) -> Result<grit_lib::remote::transfer::PushOutcome> {
    open_push_destination(config, remote_name)
        .map_err(map_remote_error)?
        .push(
            repo,
            specs,
            PushOptions {
                dry_run: wire.dry_run,
                atomic: wire.atomic,
                push_options: wire.push_options.to_vec(),
                remote_ref_tips: wire.remote_ref_tips.clone(),
                tracking_remote: Some(remote_name.to_owned()),
                ..Default::default()
            },
            progress,
            &NeverCancel,
            Some(factory),
        )
        .map_err(map_remote_error)
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

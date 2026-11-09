//! `grit push` — publish refs to a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::progress::CancelToken;
use grit_lib::remote::push_plan::{
    expand_repository_shorthand, open_push_destination, push_follow_tags_config,
    resolve_push_invocation, PushPlanKind, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::redact_url_for_display;
use grit_lib::remote::transfer::PushOptions;
use grit_lib::remote::DefaultHttpClientFactory;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::context;
use crate::progress::summary::{transfer_header, TransferSummary};
use crate::progress::TerminalProgress;
use crate::quiet_args::QuietArgs;
pub use crate::render::push::{
    PushKind, PushOutcome, PushRefOutcomeStatus, PushRefResult, PushSharedBranch,
    PushedToIntegrated,
};
use grit_lib::remote::transfer::TransferStats;

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

impl From<&PushRefStatus> for PushRefOutcomeStatus {
    fn from(status: &PushRefStatus) -> Self {
        match status {
            PushRefStatus::Ok => Self::Ok,
            PushRefStatus::UpToDate => Self::UpToDate,
            _ => Self::Rejected,
        }
    }
}

/// Wire and local options forwarded to [`PushOptions`].
struct PushWireOptions<'a> {
    dry_run: bool,
    atomic: bool,
    push_options: &'a [String],
    remote_ref_tips: Option<Vec<ObjectId>>,
    no_verify: bool,
}

/// Positional `[<remote>] [<refspec>...]` plus flags.
#[allow(clippy::too_many_arguments)]
pub fn run(
    tags: bool,
    delete_refs: bool,
    all: bool,
    mirror: bool,
    prune: bool,
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
    human_interactive: bool,
    no_verify: bool,
) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(
        &crate::context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &crate::context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;

    // `grit push <owner>/<project> main` pushes to that repository on the default grit host.
    let shorthand_url = remote_flag
        .is_none()
        .then(|| expand_repository_shorthand(&config, positionals, &auth::default_grit_host_url()))
        .flatten();
    let mut positionals = positionals.to_vec();
    if let Some(url) = &shorthand_url {
        positionals[0] = url.clone();
    }
    let invocation = resolve_push_invocation(&config, remote_flag.as_deref(), &positionals)
        .map_err(plan_error)?;

    let factory = crate::context::http_client_factory();
    let plan_ctx = repo
        .remotes()
        .push_plan_context(invocation.remote.as_deref(), Some(&factory))
        .map_err(plan_error)?;

    let force_mode = repo
        .remotes()
        .force_mode(force, force_with_lease)
        .map_err(plan_error)?;

    let follow = follow_tags || push_follow_tags_config(&config);

    let push_request = PushRequest {
        remote: invocation.remote,
        refspecs: invocation.refspecs,
        tags,
        delete_refs,
        delete: Vec::new(),
        force: force_mode,
        follow_tags: follow,
        set_upstream,
        all,
        mirror,
        prune,
    };

    let mut plan = repo
        .remotes()
        .resolve_push(&push_request, &plan_ctx)
        .map_err(plan_error)?;
    // Your head of a shared branch is yours alone (grit share): replacing it
    // after an amend or rebase needs no --force, only that it still be the
    // head this clone last fetched.
    for spec in plan.specs.iter_mut() {
        let shared = spec
            .dst
            .strip_prefix("refs/heads/")
            .is_some_and(|b| crate::commands::sync::is_shared(&config, b));
        if shared && !spec.force && !spec.delete {
            // A clone of an organization's project pushes `origin` to your
            // space (its pushurl), which the `space` remote fetches.
            let space = crate::commands::sync::SPACE_REMOTE;
            let pushurl =
                crate::context::config_string(&config, &format!("remote.{}.pushurl", plan.remote));
            let tracking = if pushurl.is_some()
                && pushurl == crate::context::config_string(&config, &format!("remote.{space}.url"))
            {
                space
            } else {
                plan.remote.as_str()
            };
            repo.remotes().apply_tracking_lease(tracking, spec);
        }
    }

    let wire = PushWireOptions {
        dry_run,
        atomic,
        push_options,
        remote_ref_tips: plan_ctx.remote_tips,
        no_verify,
    };
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = quiet.progress(human_interactive).with_overall(false);
    let url = crate::context::config_string(&config, &format!("remote.{}.url", plan.remote))
        .or_else(|| shorthand_url.clone());
    if progress.is_drawing() {
        eprintln!(
            "{}",
            transfer_header(
                "Pushing",
                &plan.remote,
                url.as_deref(),
                progress.uses_color()
            )
        );
    }
    let report = match push_once(
        &repo,
        &config,
        &plan.remote,
        &plan.specs,
        &wire,
        &factory,
        &mut progress,
        &crate::interrupt::process_cancel_token(),
    ) {
        Ok(report) => report,
        Err(err) => {
            let url =
                crate::context::config_string(&config, &format!("remote.{}.url", plan.remote))
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
                    &crate::interrupt::process_cancel_token(),
                )?
            } else {
                return Err(err);
            }
        }
    };

    let upstream_set = if push_request.set_upstream && !dry_run {
        repo.remotes()
            .set_upstream_after_push(&plan.remote, &plan.specs, &report.results)
            .map_err(|e| anyhow::Error::msg(e.to_string()))?
    } else {
        Vec::new()
    };

    let origin_added = match shorthand_url {
        Some(url) if report.has_updates() && !dry_run => repo
            .remotes()
            .add_origin_if_none(&url)
            .map_err(map_remote_error)?
            .then_some(url),
        _ => None,
    };

    let rejected = report.has_rejections();
    let results: Vec<PushRefResult> = report
        .results
        .iter()
        .map(|result| {
            let status = PushRefOutcomeStatus::from(&result.status);
            let reason = match result.status {
                PushRefStatus::Ok | PushRefStatus::UpToDate => None,
                PushRefStatus::RejectNonFastForward => {
                    Some("not a fast-forward — run `grit pull` first".to_owned())
                }
                PushRefStatus::RejectNeedsForce => Some(
                    "not a fast-forward — run `grit push --force-with-lease` after integrating, or `grit pull` first"
                        .to_owned(),
                ),
                PushRefStatus::RejectStale => Some("stale".to_owned()),
                PushRefStatus::RejectRemoteRefMissing => Some("remote_ref_missing".to_owned()),
                PushRefStatus::RejectAlreadyExists => {
                    if result.message.as_deref() == Some("tag already exists") {
                        Some("tag_exists".to_owned())
                    } else {
                        Some("already_exists".to_owned())
                    }
                }
                _ => Some(
                    result
                        .message
                        .clone()
                        .unwrap_or_else(|| "rejected".to_owned()),
                ),
            };
            PushRefResult {
                ref_name: result.remote_ref.clone(),
                status,
                reason,
                old_oid: result.old_oid.map(|o| o.to_hex()),
                new_oid: result.new_oid.map(|o| o.to_hex()),
                forced: result.forced,
                src: result.local_ref.clone(),
                deleted: result.deletion.then_some(true),
                detail: result.status.clone(),
            }
        })
        .collect();

    let branch = match plan.kind {
        PushPlanKind::Branch | PushPlanKind::Delete => plan.branch_label,
        _ => None,
    };

    // After replacing your head of a shared branch, see where the others are.
    let mut shared = Vec::new();
    let mut pushed_to_integrated = Vec::new();
    if !dry_run {
        for result in &results {
            let Some(name) = result.ref_name.strip_prefix("refs/heads/") else {
                continue;
            };
            let pushed = matches!(
                result.status,
                PushRefOutcomeStatus::Ok | PushRefOutcomeStatus::UpToDate
            );
            if !pushed || result.deleted == Some(true) {
                continue;
            }
            // A branch made again: was it shared, and integrated since?
            if result.old_oid.is_none() {
                if let Some(report) = integrated_again(&repo, &config, &plan.remote, name) {
                    let _ = crate::commands::sync::unmark_shared(&repo, name);
                    pushed_to_integrated.push(PushedToIntegrated {
                        branch: name.to_owned(),
                        report,
                    });
                    continue;
                }
            }
            if !crate::commands::sync::is_shared(&config, name) {
                continue;
            }
            let space = crate::commands::sync::SPACE_REMOTE;
            let pushurl =
                crate::context::config_string(&config, &format!("remote.{}.pushurl", plan.remote));
            let tracking = if pushurl.is_some()
                && pushurl == crate::context::config_string(&config, &format!("remote.{space}.url"))
            {
                space.to_owned()
            } else {
                plan.remote.clone()
            };
            if crate::commands::sync::fetch_shared_heads(&repo, &tracking, false).is_err() {
                continue;
            }
            let heads_now =
                |ours: Option<ObjectId>| -> Vec<crate::render::shared_branch::SharedHead> {
                    crate::commands::sync::shared_heads(&repo, &tracking, name)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(user, tip)| {
                            crate::commands::sync::SharedHead::new(
                                &repo, &tracking, name, user, ours, tip,
                            )
                        })
                        .collect()
                };
            let mut ours = repo.references().resolve(format!("refs/heads/{name}")).ok();
            let mut heads = heads_now(ours);
            // Heads that go together with yours without losing anything:
            // combine them here, and push the result.
            let mut combined = Vec::new();
            for h in &heads {
                let (Some(mine), Some(d)) = (ours, &h.divergence) else {
                    continue;
                };
                if d.needs_decision {
                    continue;
                }
                let Ok(tip) = h.tip.parse::<ObjectId>() else {
                    continue;
                };
                if let Some(head) =
                    crate::commands::share::auto_combine(&repo, &tracking, name, &h.user, mine, tip)
                {
                    ours = Some(head);
                    combined.push(h.user.clone());
                }
            }
            let mut head = None;
            if !combined.is_empty() {
                let again = crate::commands::sync::push(&plan.remote, &[name.to_owned()], false)?;
                if let Some(Some(why)) = again.get(name) {
                    anyhow::bail!(
                        "combined {name} with {}'s head here, but pushing it was refused: {why}",
                        combined.join(" and ")
                    );
                }
                head = ours.map(|o| o.to_hex());
                heads = heads_now(ours);
            }
            shared.push(PushSharedBranch {
                branch: name.to_owned(),
                heads,
                combined,
                head,
            });
        }
    }

    let mut transfer = TransferSummary::new(
        push_transfer_stats(progress.phases(), &report.sent),
        progress.phases().to_vec(),
        progress.elapsed(),
    );
    transfer.transfer_sent = true;

    Ok(PushOutcome {
        remote: redact_url_for_display(&plan.remote),
        kind: plan.kind.into(),
        branch,
        results,
        rejected,
        dry_run,
        upstream_set: (!upstream_set.is_empty()).then_some(upstream_set),
        origin_added,
        remote_messages: report.remote_messages,
        shared,
        pushed_to_integrated,
        transfer,
    })
}

/// When `branch`, just made on `remote`, was shared there and integrated
/// since, and brings integrated commits: the report. Asking the server is
/// best-effort; a push never fails on it.
fn integrated_again(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote: &str,
    branch: &str,
) -> Option<crate::render::shared_branch::IntegratedJson> {
    let url = crate::context::config_string(config, &format!("remote.{remote}.pushurl"))
        .or_else(|| crate::context::config_string(config, &format!("remote.{remote}.url")))?;
    let (server, project) = grit_lib::remote::grit_server_space::hosted_project(&url)?;
    let tip = repo
        .references()
        .resolve(format!("refs/heads/{branch}"))
        .ok()?;
    crate::commands::share::integrated_on(repo, config, &server, &project, branch, tip)
        .ok()
        .flatten()
}

fn plan_error(err: grit_lib::error::Error) -> anyhow::Error {
    anyhow::Error::new(err)
}

fn push_transfer_stats(
    records: &[crate::progress::PhaseRecord],
    sent: &TransferStats,
) -> TransferStats {
    use grit_lib::progress::{ProgressPhase, ProgressUnit};
    let mut objects = 0u64;
    let mut bytes = 0u64;
    for rec in records {
        match (rec.phase, rec.unit) {
            (ProgressPhase::CopyingObjects, ProgressUnit::Objects) => {
                objects = objects.max(rec.count);
            }
            (ProgressPhase::SendingObjects, ProgressUnit::Bytes) => {
                bytes = bytes.max(rec.count);
            }
            _ => {}
        }
    }
    TransferStats {
        objects: objects.max(sent.objects),
        bytes: bytes.max(sent.bytes),
        kinds: sent.kinds,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    specs: &[grit_lib::remote::transfer::PushRefSpec],
    wire: &PushWireOptions<'_>,
    factory: &DefaultHttpClientFactory,
    progress: &mut TerminalProgress,
    cancel: &CancelToken,
) -> Result<grit_lib::remote::transfer::PushOutcome> {
    let remote = open_push_destination(config, remote_name).map_err(map_remote_error)?;
    let options = PushOptions {
        dry_run: wire.dry_run,
        atomic: wire.atomic,
        push_options: wire.push_options.to_vec(),
        remote_ref_tips: wire.remote_ref_tips.clone(),
        no_verify: wire.no_verify,
        ..Default::default()
    };
    remote
        .push(repo, specs, options, progress, cancel, Some(factory))
        .map_err(map_remote_error)
}

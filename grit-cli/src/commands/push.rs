//! `grit push` — publish refs to a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::remote::push_plan::{
    apply_set_upstream_after_push, expand_repository_shorthand, load_push_plan_context,
    open_push_destination, parse_force_mode, push_follow_tags_config, resolve_push,
    resolve_push_invocation, PushPlanError, PushPlanKind, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::redact_url_for_display;
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
use crate::progress::summary::{stdout_color, transfer_header, TransferSummary};
use crate::progress::TerminalProgress;
use crate::quiet_args::QuietArgs;
use grit_lib::progress::Cancel;
use grit_lib::remote::transfer::TransferStats;

/// Result of `grit push`: the per-ref outcomes for the remote.
#[derive(Serialize, JsonSchema)]
pub struct PushOutcome {
    /// Remote name, or the URL pushed to with any credentials removed.
    pub remote: String,
    /// High-level push kind (`branch`, `tags`, `delete`, `refspecs`).
    pub kind: PushKind,
    /// Local branch short name when [`Self::kind`] is `branch` or `delete` (legacy `branch` field).
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
    /// URL saved as `origin`: a `<owner>/<project>` push from a repository with no remotes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub origin_added: Option<String>,
    /// Hook and diagnostic lines from the remote (`remote: …` in human output).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip_serializing_if = "Vec::is_empty")]
    pub remote_messages: Vec<String>,
    /// For each shared branch pushed (`grit share`): the other members'
    /// heads, fetched after the push and compared with yours.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub shared: Vec<PushSharedBranch>,
    /// Branches this push made again after a share of them was integrated
    /// (scenario `pushed_to_integrated`): nobody shares them now. Each has
    /// the options `grit share resolve` takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub pushed_to_integrated: Vec<PushedToIntegrated>,
    /// Pack build and send timing, for the human summary after ref results.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

/// A shared branch a push replaced your head of.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PushSharedBranch {
    /// The branch.
    pub branch: String,
    /// The other members' heads, compared with yours.
    pub heads: Vec<crate::commands::sync::SharedHead>,
    /// Members whose diverged heads went together with yours without losing
    /// anything: combined into your branch here, and pushed again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub combined: Vec<String>,
    /// Your head afterwards, when combining moved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub head: Option<String>,
}

/// A branch pushed again after a share of it was integrated.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PushedToIntegrated {
    /// The branch.
    pub branch: String,
    /// What happened, and the options.
    #[serde(flatten)]
    pub report: crate::commands::share::IntegratedJson,
}

impl PushOutcome {
    /// Whether a shared branch pushed needs someone to decide what to do.
    pub fn needs_decision(&self) -> bool {
        !self.pushed_to_integrated.is_empty()
            || self.shared.iter().any(|s| {
                s.heads
                    .iter()
                    .any(crate::commands::sync::SharedHead::needs_decision)
            })
    }
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
    /// True when this ref was deleted on the remote (omit when false).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    /// The library's status, which [`Self::status`] and [`Self::reason`] summarize.
    #[serde(skip)]
    #[schemars(skip)]
    pub detail: PushRefStatus,
}

/// Short display name for a pushed remote ref (`refs/heads/foo` → `foo`).
pub(crate) fn ref_short_display(full_ref: &str) -> &str {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = full_ref.strip_prefix(prefix) {
            return rest;
        }
    }
    full_ref
}

pub(crate) fn push_result_source_label(result: &PushRefResult, kind: PushKind) -> &str {
    if let Some(src) = result.src.as_deref() {
        return ref_short_display(src);
    }
    if kind == PushKind::Tags {
        return "--tags";
    }
    ref_short_display(&result.ref_name)
}

impl HumanRender for PushOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for result in &self.results {
            let target = format!("{} {}", self.remote, result.ref_name);
            match result.status {
                PushRefOutcomeStatus::Ok => {
                    if result.deleted == Some(true) {
                        let short = ref_short_display(&result.ref_name);
                        if self.dry_run {
                            outln!(w, "  would delete {short} from {}", self.remote);
                        } else {
                            outln!(w, "  deleted {short} from {}", self.remote);
                        }
                    } else {
                        let label = push_result_source_label(result, self.kind);
                        if self.dry_run {
                            outln!(w, "  would push {label} → {target}");
                        } else {
                            outln!(w, "  pushed {label} → {target}");
                        }
                    }
                }
                PushRefOutcomeStatus::UpToDate => outln!(w, "  {target} already up to date"),
                PushRefOutcomeStatus::Rejected => match result.detail {
                    PushRefStatus::RejectStale => outln!(
                        w,
                        "  rejected {target}: stale remote ref — run `grit fetch --prune` and try again"
                    ),
                    PushRefStatus::RejectRemoteRefMissing => {
                        outln!(w, "  rejected {target}: remote ref does not exist")
                    }
                    PushRefStatus::RejectAlreadyExists => {
                        if result.reason.as_deref() == Some("tag_exists") {
                            outln!(
                                w,
                                "  rejected {target}: tag already exists (use `--force` to replace)"
                            );
                        } else {
                            outln!(
                                w,
                                "  rejected {target}: created on the remote meanwhile — run `grit fetch` and try again"
                            );
                        }
                    }
                    _ => {
                        let detail = result.reason.as_deref().unwrap_or("rejected");
                        outln!(w, "  rejected {target}: {detail}");
                    }
                },
            }
        }
        if let Some(url) = &self.origin_added {
            outln!(w, "  added remote origin → {url}");
        }
        for s in &self.shared {
            if !s.combined.is_empty() {
                outln!(
                    w,
                    "✓ combined {} with {}'s head (nothing lost) and pushed it{}",
                    s.branch,
                    s.combined.join("'s and "),
                    s.head
                        .as_deref()
                        .map(|h| format!(": {}", grit_lib::objects::short_hex(h)))
                        .unwrap_or_default()
                );
            }
            for h in &s.heads {
                if s.combined.contains(&h.user) {
                    // Nothing to decide: their next sync takes the result.
                    outln!(
                        w,
                        "  {}: {}'s head {} is in yours now; their next sync takes it",
                        s.branch,
                        h.user,
                        grit_lib::objects::short_hex(&h.tip),
                    );
                    continue;
                }
                outln!(
                    w,
                    "  {}: {}'s head {} {}",
                    s.branch,
                    h.user,
                    grit_lib::objects::short_hex(&h.tip),
                    h.relation.words()
                );
                if let Some(d) = &h.divergence {
                    d.render_human(w, "    ")?;
                }
            }
        }
        for p in &self.pushed_to_integrated {
            p.report.render_human(w, &p.branch)?;
        }
        if self.transfer.received_anything() {
            outln!(w);
            self.transfer.render_rows(w, stdout_color())?;
        }
        Ok(())
    }
}

impl MarkdownRender for PushOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_remote_push_messages(w, &self.remote_messages)?;
        markdown::print_push_results(w, &self.remote, self.kind, self.dry_run, &self.results)?;
        for s in &self.shared {
            writeln!(w, "\n## Shared branch `{}`\n", s.branch)?;
            if !s.combined.is_empty() {
                writeln!(
                    w,
                    "Combined with {}'s head (nothing lost) and pushed the result{}.\n",
                    s.combined.join("'s and "),
                    s.head
                        .as_deref()
                        .map(|h| format!(", `{}`", grit_lib::objects::short_hex(h)))
                        .unwrap_or_default()
                )?;
            }
            if s.heads.is_empty() {
                writeln!(w, "Nobody else has pushed a head yet.")?;
            }
            for h in &s.heads {
                if s.combined.contains(&h.user) {
                    writeln!(
                        w,
                        "- `{}`'s head `{}`: in yours now; their next sync takes it",
                        h.user,
                        grit_lib::objects::short_hex(&h.tip),
                    )?;
                    continue;
                }
                writeln!(
                    w,
                    "- `{}`'s head `{}`: {}",
                    h.user,
                    grit_lib::objects::short_hex(&h.tip),
                    h.relation.words()
                )?;
                if let Some(d) = &h.divergence {
                    d.render_markdown(w, "  ")?;
                }
            }
            if s.heads
                .iter()
                .any(crate::commands::sync::SharedHead::needs_decision)
            {
                writeln!(w, "\nExit status 3: pushed, decision needed.")?;
            }
        }
        for p in &self.pushed_to_integrated {
            writeln!(w, "\n## Pushed to an integrated branch: `{}`\n", p.branch)?;
            p.report.render_markdown(w, &p.branch)?;
            writeln!(w, "\nExit status 3: pushed, decision needed.")?;
        }
        if let Some(url) = &self.origin_added {
            writeln!(w, "\nAdded remote `origin` → `{url}`.")?;
        }
        Ok(())
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
    mode: OutputMode,
    no_verify: bool,
) -> Result<PushOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
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
    let plan_ctx =
        load_push_plan_context(&repo, &config, invocation.remote.as_deref(), Some(&factory))
            .map_err(plan_error)?;

    let force_mode = parse_force_mode(&repo, force, force_with_lease).map_err(plan_error)?;

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

    let mut plan = resolve_push(&repo, &config, &push_request, &plan_ctx).map_err(plan_error)?;
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
            let pushurl = config.get(&format!("remote.{}.pushurl", plan.remote));
            let tracking =
                if pushurl.is_some() && pushurl == config.get(&format!("remote.{space}.url")) {
                    space
                } else {
                    plan.remote.as_str()
                };
            grit_lib::remote::push_plan::apply_tracking_lease(&repo, &config, tracking, spec);
        }
    }

    let wire = PushWireOptions {
        dry_run,
        atomic,
        push_options,
        remote_ref_tips: plan_ctx.remote_tips,
        no_verify,
    };
    let interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = quiet.progress(mode).with_overall(false);
    let url = config
        .get(&format!("remote.{}.url", plan.remote))
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
        &interrupt,
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
                    &interrupt,
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
            let pushurl = config.get(&format!("remote.{}.pushurl", plan.remote));
            let tracking =
                if pushurl.is_some() && pushurl == config.get(&format!("remote.{space}.url")) {
                    space.to_owned()
                } else {
                    plan.remote.clone()
                };
            if crate::commands::sync::fetch_shared_heads(&repo, &tracking, mode).is_err() {
                continue;
            }
            let heads_now = |ours: Option<ObjectId>| -> Vec<crate::commands::sync::SharedHead> {
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
            let mut ours =
                grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{name}")).ok();
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
                let again = crate::commands::sync::push(&plan.remote, &[name.to_owned()], mode)?;
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
) -> Option<crate::commands::share::IntegratedJson> {
    let url = config
        .get(&format!("remote.{remote}.pushurl"))
        .or_else(|| config.get(&format!("remote.{remote}.url")))?;
    let (server, project) = grit_lib::remote::grit_server_space::hosted_project(&url)?;
    let tip = grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{branch}")).ok()?;
    crate::commands::share::integrated_on(repo, config, &server, &project, branch, tip)
        .ok()
        .flatten()
}

fn plan_error(err: PushPlanError) -> anyhow::Error {
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
    cancel: &dyn Cancel,
) -> Result<grit_lib::remote::transfer::PushOutcome> {
    let remote = open_push_destination(config, remote_name).map_err(map_remote_error)?;
    let options = PushOptions {
        dry_run: wire.dry_run,
        atomic: wire.atomic,
        push_options: wire.push_options.to_vec(),
        remote_ref_tips: wire.remote_ref_tips.clone(),
        tracking_remote: Some(remote_name.to_owned()),
        no_verify: wire.no_verify,
        ..Default::default()
    };
    remote
        .push(repo, specs, options, progress, cancel, Some(factory))
        .map_err(map_remote_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_status_serializes_snake_case() {
        for (detail, expected) in [
            (PushRefStatus::Ok, "ok"),
            (PushRefStatus::UpToDate, "up_to_date"),
            (PushRefStatus::RejectFetchFirst, "rejected"),
        ] {
            let result = result_with(detail, None);
            let value = serde_json::to_value(&result).expect("json");
            assert_eq!(value["status"].as_str(), Some(expected));
            assert!(value.get("detail").is_none(), "{value}");
        }
    }

    fn result_with(detail: PushRefStatus, reason: Option<&str>) -> PushRefResult {
        PushRefResult {
            ref_name: "refs/heads/main".to_owned(),
            status: PushRefOutcomeStatus::from(&detail),
            reason: reason.map(str::to_owned),
            old_oid: None,
            new_oid: None,
            forced: false,
            src: None,
            deleted: None,
            detail,
        }
    }

    #[test]
    fn human_rejection_wording_follows_the_library_status() {
        let ctx = RenderContext {
            now: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp"),
            mode: OutputMode::Human,
        };
        let render = |result: PushRefResult| {
            let outcome = PushOutcome {
                remote: "origin".to_owned(),
                kind: PushKind::Branch,
                branch: Some("main".to_owned()),
                results: vec![result],
                rejected: true,
                dry_run: false,
                upstream_set: None,
                origin_added: None,
                remote_messages: Vec::new(),
                shared: Vec::new(),
                pushed_to_integrated: Vec::new(),
                transfer: TransferSummary::default(),
            };
            let mut out = Vec::new();
            outcome.render_human(&mut out, &ctx).expect("render");
            String::from_utf8(out).expect("utf8")
        };

        let stale = render(result_with(PushRefStatus::RejectStale, Some("stale")));
        assert!(
            stale.contains("stale remote ref — run `grit fetch --prune`"),
            "{stale}"
        );
        let missing = render(result_with(
            PushRefStatus::RejectRemoteRefMissing,
            Some("remote_ref_missing"),
        ));
        assert!(missing.contains("remote ref does not exist"), "{missing}");
        let exists = render(result_with(
            PushRefStatus::RejectAlreadyExists,
            Some("already_exists"),
        ));
        assert!(
            exists.contains("created on the remote meanwhile"),
            "{exists}"
        );

        let declined = render(result_with(PushRefStatus::RemoteRejected, Some("stale")));
        assert!(
            declined.contains("rejected origin refs/heads/main: stale"),
            "a remote's own message is shown as given: {declined}"
        );
    }
}

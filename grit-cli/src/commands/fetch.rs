//! `grit fetch` — download refs and objects from a remote (default: the current
//! branch's remote, else `origin`).

use crate::commands::fetch_args::FetchArgs;
use crate::output::OutputMode;
use crate::progress::summary::{stdout_color, transfer_header, TransferSummary};
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::short_hex;
use grit_lib::progress::CancelToken;
use grit_lib::remote::fetch_request::{resolve_fetch_invocation, resolve_remote_argument};
use grit_lib::remote::redact_url_for_display;
use grit_lib::remote::transfer::{
    resolve_fetch_prune, resolve_fetch_tags, FetchDepth, FetchOptions,
    FetchOutcome as LibFetchOutcome, TagMode, TransferStats, UpdateKind, UpdateMode,
};
use grit_lib::remote::{DefaultHttpClientFactory, RemoteError};
use grit_lib::state::{resolve_head_for_repository, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::auth;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit fetch`: the refs that changed.
#[derive(Serialize, JsonSchema)]
pub struct FetchOutcome {
    /// Remote name, or the URL fetched from with any credentials removed.
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
    /// True when the fetch was a dry run (no refs or objects written).
    pub dry_run: bool,
    /// True when any ref update was rejected (exit 1 when set).
    pub rejected: bool,
    /// Shallow boundary changes when a depth/deepen/unshallow fetch ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub shallow: Option<FetchShallowDelta>,
    /// Promisor remotes the server advertised, and whether this fetch accepted each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_remotes: Vec<crate::commands::promisor_json::PromisorRemoteJson>,
    /// Remote fields stored from the advertisement (`promisor.storeFields`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub stored_fields: Vec<crate::commands::promisor_json::StoredFieldJson>,
    /// Promisor config or advertised values that were ignored or could not be used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_warnings: Vec<crate::commands::promisor_json::PromisorWarningJson>,
    /// What was received and where the time went, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
    /// Refs this fetch changed, or on a dry run would change.
    #[serde(skip)]
    #[schemars(skip)]
    pub changed: usize,
}

/// Shallow boundaries added or removed by this fetch (`--json` only).
#[derive(Serialize, JsonSchema)]
pub struct FetchShallowDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Result of `grit fetch --all`.
#[derive(Serialize, JsonSchema)]
pub struct FetchAllOutcome {
    pub remotes: Vec<FetchAllRemoteOutcome>,
    pub updated: usize,
    pub dry_run: bool,
    /// Totals across every remote, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

/// One remote's result inside [`FetchAllOutcome`].
#[derive(Serialize, JsonSchema)]
pub struct FetchAllRemoteOutcome {
    /// Remote name.
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Refs this remote's fetch changed, or on a dry run would change.
    #[serde(skip)]
    #[schemars(skip)]
    pub changed: usize,
    /// True when any of this remote's ref updates was rejected.
    #[serde(skip)]
    #[schemars(skip)]
    pub rejected: bool,
}

impl FetchAllRemoteOutcome {
    /// This remote's result in the single-remote shape, for rendering.
    fn as_single(&self, dry_run: bool) -> FetchOutcome {
        FetchOutcome {
            remote: self.remote.clone(),
            updates: self.updates.clone(),
            updated: self.updated,
            dry_run,
            rejected: self.rejected,
            shallow: None,
            promisor_remotes: Vec::new(),
            stored_fields: Vec::new(),
            promisor_warnings: Vec::new(),
            transfer: TransferSummary::default(),
            changed: self.changed,
        }
    }
}

/// Human / JSON output for either single-remote or `--all` fetch.
pub enum FetchRunOutcome {
    One(FetchOutcome),
    All(FetchAllOutcome),
}

impl FetchRunOutcome {
    #[must_use]
    pub fn exit_nonzero(&self) -> bool {
        match self {
            Self::One(o) => o.rejected,
            Self::All(o) => o.remotes.iter().any(|r| r.error.is_some() || r.rejected),
        }
    }
}

impl HumanRender for FetchAllOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for remote in &self.remotes {
            if let Some(err) = &remote.error {
                outln!(w, "From {}:", remote.remote);
                outln!(w, "  error: {err}");
                continue;
            }
            remote.as_single(self.dry_run).render_human(w, ctx)?;
        }
        render_transfer(&self.transfer, w)
    }
}

impl MarkdownRender for FetchAllOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        for remote in &self.remotes {
            if let Some(err) = &remote.error {
                outln!(w, "## {}", remote.remote);
                outln!(w, "- error: {err}");
                continue;
            }
            remote.as_single(self.dry_run).render_markdown(w, ctx)?;
        }
        Ok(())
    }
}

/// Per-ref fetch status in JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FetchUpdateStatus {
    New,
    FastForward,
    Forced,
    Pruned,
    Rejected,
    UpToDate,
}

impl From<UpdateKind> for FetchUpdateStatus {
    fn from(kind: UpdateKind) -> Self {
        match kind {
            UpdateKind::New => Self::New,
            UpdateKind::FastForward => Self::FastForward,
            UpdateKind::Forced => Self::Forced,
            UpdateKind::Pruned => Self::Pruned,
            UpdateKind::Rejected => Self::Rejected,
            UpdateKind::UpToDate => Self::UpToDate,
        }
    }
}

/// One updated tracking ref. `old_oid`/`new_oid` are full hex, or `null` for a
/// newly-created (`old_oid`) or deleted (`new_oid`) ref.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FetchUpdate {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub remote_ref: String,
    pub status: FetchUpdateStatus,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    /// Rejection detail, present only when `status == "rejected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HumanRender for FetchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for update in &self.updates {
            if update.status == FetchUpdateStatus::Rejected {
                let reason = update.reason.as_deref().unwrap_or("ref update rejected");
                outln!(w, "  ! rejected {}  {}", update.ref_name, reason);
                continue;
            }
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            if update.remote_ref.is_empty() || update.remote_ref == update.ref_name {
                outln!(w, "  {}  {from} → {to}", update.ref_name);
            } else {
                outln!(
                    w,
                    "  {}  ← {}  {from} → {to}",
                    update.ref_name,
                    update.remote_ref
                );
            }
        }
        if self.rejected {
            outln!(w, "Some ref updates from {} were rejected.", self.remote);
        } else if self.changed == 0 {
            outln!(w, "Already up to date with {}.", self.remote);
        } else {
            outln!(
                w,
                "{} {} update{} from {}.",
                if self.dry_run {
                    "Would fetch"
                } else {
                    "Fetched"
                },
                self.changed,
                plural(self.changed),
                self.remote
            );
        }
        for line in crate::commands::promisor_json::human_lines(
            &self.promisor_remotes,
            &self.stored_fields,
            &self.promisor_warnings,
        ) {
            outln!(w, "  promisor  {line}");
        }
        render_transfer(&self.transfer, w)
    }
}

/// The time bar and data/objects rows under a fetch's ref list, when the
/// fetch received anything.
fn render_transfer(transfer: &TransferSummary, w: &mut dyn Write) -> Result<()> {
    if !transfer.received_anything() {
        return Ok(());
    }
    outln!(w);
    transfer.render_rows(w, stdout_color())
}

impl MarkdownRender for FetchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_fetch_updates(w, &self.remote, &self.updates, self.changed, self.dry_run)?;
        if let Some(line) = crate::commands::promisor_json::markdown_line(&self.promisor_remotes) {
            outln!(w, "{line}");
        }
        for s in &self.stored_fields {
            outln!(w, "- **stored**: `{}` for `{}`", s.field, s.remote);
        }
        for line in crate::commands::promisor_json::markdown_warning_lines(&self.promisor_warnings)
        {
            outln!(w, "{line}");
        }
        Ok(())
    }
}

pub fn run(args: &FetchArgs, mode: OutputMode) -> Result<FetchRunOutcome> {
    if args.all {
        return run_all(args, mode).map(FetchRunOutcome::All);
    }
    run_one(args, mode).map(FetchRunOutcome::One)
}

fn run_one(args: &FetchArgs, mode: OutputMode) -> Result<FetchOutcome> {
    let repo = context::discover()?;
    run_one_in(&repo, args, mode)
}

/// Fetch into `repo`, wherever the process is (one remote; `args.all` is
/// not looked at).
pub(crate) fn run_one_in(
    repo: &grit_lib::repo::Repository,
    args: &FetchArgs,
    mode: OutputMode,
) -> Result<FetchOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let head = resolve_head_for_repository(repo).ok();
    let invocation = resolve_fetch_invocation(
        &config,
        args.remote.as_deref(),
        &args.refspecs,
        head.as_ref().and_then(HeadState::branch_name),
    );
    let remote_name = invocation.remote.clone();
    let prune_enabled = resolve_fetch_prune(&config, &remote_name, args.prune_override())
        .context("could not resolve fetch prune settings")?;
    let tags = resolve_fetch_tags(&config, &remote_name, args.tag_mode_override());

    let factory = crate::context::http_client_factory();
    // No checkout follows a fetch, so the overall bar ends with the pack.
    let mut progress = args.quiet.progress(mode).with_overall(false);
    if matches!(args.depth, Some(0)) {
        return Err(anyhow::anyhow!("--depth requires a positive integer"));
    }
    if matches!(args.deepen, Some(0)) {
        return Err(anyhow::anyhow!("--deepen requires a positive integer"));
    }
    let fetch_depth = args.fetch_depth();
    if args.depth.is_some() && fetch_depth.is_none() {
        return Err(anyhow::anyhow!("--depth requires a positive integer"));
    }
    if args.deepen.is_some() && fetch_depth.is_none() {
        return Err(anyhow::anyhow!("--deepen requires a positive integer"));
    }
    let url = crate::context::config_string(&config, &format!("remote.{remote_name}.url"));
    if progress.is_drawing() {
        eprintln!(
            "{}",
            transfer_header(
                "Fetching",
                &remote_name,
                url.as_deref(),
                progress.uses_color()
            )
        );
    }
    let result = match fetch_once(
        repo,
        &config,
        &invocation,
        tags,
        prune_enabled,
        args.dry_run,
        fetch_depth,
        args.force,
        &factory,
        &mut progress,
        &crate::interrupt::process_cancel_token(),
    ) {
        Ok(result) => result,
        Err(err) => {
            if auth::offer_reauth(&err, url.as_deref().unwrap_or_default())? {
                fetch_once(
                    repo,
                    &config,
                    &invocation,
                    tags,
                    prune_enabled,
                    args.dry_run,
                    fetch_depth,
                    args.force,
                    &factory,
                    &mut progress,
                    &crate::interrupt::process_cancel_token(),
                )?
            } else {
                return Err(err);
            }
        }
    };

    let refs = remote_outcome(remote_name, &result, args.dry_run);
    let shallow = shallow_json(&result, fetch_depth.is_some());
    let transfer = TransferSummary::new(
        result.transfer,
        progress.phases().to_vec(),
        progress.elapsed(),
    )
    .with_milestones(progress.milestones().to_vec());

    Ok(FetchOutcome {
        remote: redact_url_for_display(&refs.remote),
        updates: refs.updates,
        updated: refs.updated,
        dry_run: args.dry_run,
        rejected: refs.rejected,
        changed: refs.changed,
        shallow,
        promisor_remotes: crate::commands::promisor_json::promisor_remotes(
            result.promisor.as_ref(),
        ),
        stored_fields: crate::commands::promisor_json::stored_fields(result.promisor.as_ref()),
        promisor_warnings: crate::commands::promisor_json::warnings(result.promisor.as_ref()),
        transfer,
    })
}

fn shallow_json(
    result: &grit_lib::remote::transfer::FetchOutcome,
    depth_request: bool,
) -> Option<FetchShallowDelta> {
    if !depth_request && result.new_shallow.is_empty() && result.new_unshallow.is_empty() {
        return None;
    }
    Some(FetchShallowDelta {
        added: result.new_shallow.iter().map(|o| o.to_hex()).collect(),
        removed: result.new_unshallow.iter().map(|o| o.to_hex()).collect(),
    })
}

fn run_all(args: &FetchArgs, mode: OutputMode) -> Result<FetchAllOutcome> {
    let _interrupt = crate::interrupt::InterruptCancel::install();
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
    let factory = crate::context::http_client_factory();
    let mut progress = args.quiet.progress(mode);
    let mut base = FetchOptions::new();
    base.dry_run = args.dry_run;
    base.fetch_depth = args.fetch_depth();
    base.force = args.force;
    base.thread_policy = repo.thread_policy();
    base.environment = crate::context::environment();
    let raw = repo
        .remotes()
        .fetch_all(
            &config,
            &base,
            args.prune_override(),
            args.tag_mode_override(),
            &mut progress,
            &crate::interrupt::process_cancel_token(),
            Some(&factory),
        )
        .context("could not fetch all remotes")?;

    let mut remotes = Vec::new();
    let mut total_updated = 0usize;
    let mut received = TransferStats::default();
    for (name, result) in raw {
        match result {
            Ok(outcome) => {
                received += outcome.transfer;
                let remote = remote_outcome(name, &outcome, args.dry_run);
                total_updated += remote.updated;
                remotes.push(remote);
            }
            Err(err) => {
                remotes.push(FetchAllRemoteOutcome {
                    remote: name,
                    updates: Vec::new(),
                    updated: 0,
                    error: Some(err.to_string()),
                    changed: 0,
                    rejected: false,
                });
            }
        }
    }
    Ok(FetchAllOutcome {
        remotes,
        updated: total_updated,
        dry_run: args.dry_run,
        transfer: TransferSummary::new(received, progress.phases().to_vec(), progress.elapsed())
            .with_milestones(progress.milestones().to_vec()),
    })
}

fn remote_outcome(
    remote: String,
    outcome: &LibFetchOutcome,
    dry_run: bool,
) -> FetchAllRemoteOutcome {
    let updates = outcome
        .reported_updates()
        .map(|update| {
            let ref_name = update
                .local_ref
                .clone()
                .unwrap_or_else(|| update.remote_ref.clone());
            let remote_ref = if update.remote_ref.is_empty() {
                ref_name.clone()
            } else {
                update.remote_ref.clone()
            };
            FetchUpdate {
                ref_name,
                remote_ref,
                status: update.mode.kind().into(),
                old_oid: update.old_oid.as_ref().map(|o| o.to_hex()),
                new_oid: update.new_oid.as_ref().map(|o| o.to_hex()),
                reason: rejection_reason(update.mode, &update.note),
            }
        })
        .collect();
    let changed = outcome.changed_ref_count();
    FetchAllRemoteOutcome {
        remote,
        updates,
        updated: if dry_run { 0 } else { changed },
        error: None,
        changed,
        rejected: outcome.has_rejections(),
    }
}

fn rejection_reason(mode: UpdateMode, note: &Option<String>) -> Option<String> {
    if note.is_some() && mode == UpdateMode::NonFastForwardRejected {
        return note.clone();
    }
    match mode {
        UpdateMode::NonFastForwardRejected => {
            Some("not a fast-forward — use a leading `+` on the refspec to force".to_owned())
        }
        UpdateMode::TagUpdateRejected => Some(
            "local tag would be replaced — use --force, or a leading + on the refspec".to_owned(),
        ),
        UpdateMode::SourceObjectNotFound => Some("source ref not found on the remote".to_owned()),
        UpdateMode::RefLockRejected => note
            .clone()
            .or_else(|| Some("unable to update local ref".to_owned())),
        _ => note.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    invocation: &grit_lib::remote::fetch_request::FetchInvocation,
    tags: TagMode,
    prune: bool,
    dry_run: bool,
    fetch_depth: Option<FetchDepth>,
    force: bool,
    factory: &DefaultHttpClientFactory,
    progress: &mut TerminalProgress,
    cancel: &CancelToken,
) -> Result<grit_lib::remote::transfer::FetchOutcome> {
    let remote = resolve_remote_argument(config, &invocation.remote).map_err(map_remote_error)?;
    remote
        .fetch(
            repo,
            {
                let mut opts = FetchOptions::new();
                opts.refspecs = invocation.user_refspecs.clone();
                opts.tags = tags;
                opts.force = force;
                opts.thread_policy = repo.thread_policy();
                opts.environment = crate::context::environment();
                opts.prune = prune;
                opts.dry_run = dry_run;
                opts.fetch_depth = fetch_depth;
                opts
            },
            progress,
            cancel,
            Some(factory),
        )
        .map_err(map_remote_fetch_error)
}

pub(crate) use crate::error_map::map_remote_error;

fn map_remote_fetch_error(err: RemoteError) -> anyhow::Error {
    map_remote_error(err)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod map_remote_error_tests {
    use grit_lib::error::Error as LibError;
    use grit_lib::objects::ObjectId;
    use grit_lib::remote::RemoteError;

    use super::map_remote_error;
    use crate::cli_messages::human_error_message;
    use crate::json_error::structured_json_payload;

    #[test]
    fn library_object_not_found_keeps_remote_display() {
        let err = map_remote_error(RemoteError::Nested(Box::new(LibError::ObjectNotFound(
            "deadbeef".into(),
        ))));
        assert!(err.downcast_ref::<LibError>().is_none());
        assert_eq!(
            human_error_message(&err),
            "object not found: deadbeef",
            "must not remap to revision-not-found wording"
        );
        assert!(structured_json_payload(&err).is_none());
    }

    #[test]
    fn library_hash_collision_stays_typed_for_json() {
        let oid = ObjectId::from_hex("f92d74e3874587aaf443d1db961d4e26dde13e9c").expect("oid");
        let err = map_remote_error(RemoteError::Nested(Box::new(LibError::HashCollision {
            oid,
        })));
        let payload = structured_json_payload(&err).expect("structured payload");
        assert_eq!(payload.kind, crate::error_kind::ErrorKind::HashCollision);
        assert_eq!(payload.oid.as_deref(), Some(oid.to_hex().as_str()));
    }

    fn lib_outcome(
        modes: &[(&str, grit_lib::remote::transfer::UpdateMode)],
    ) -> grit_lib::remote::transfer::FetchOutcome {
        use grit_lib::remote::transfer::RefUpdate;

        let oid = ObjectId::from_hex("a8e620a111111111111111111111111111111111").expect("oid");
        grit_lib::remote::transfer::FetchOutcome {
            updates: modes
                .iter()
                .map(|(name, mode)| RefUpdate {
                    remote_ref: format!("refs/heads/{name}"),
                    local_ref: Some(format!("refs/remotes/origin/{name}")),
                    old_oid: None,
                    new_oid: Some(oid),
                    mode: *mode,
                    note: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn render(outcome: &super::FetchOutcome) -> (String, String) {
        use crate::output::{HumanRender, MarkdownRender, OutputMode, RenderContext};

        let ctx = RenderContext {
            now: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp"),
            mode: OutputMode::Human,
        };
        let mut human = Vec::new();
        outcome.render_human(&mut human, &ctx).expect("human");
        let mut markdown = Vec::new();
        outcome
            .render_markdown(&mut markdown, &ctx)
            .expect("markdown");
        (
            String::from_utf8(human).expect("utf8"),
            String::from_utf8(markdown).expect("utf8"),
        )
    }

    #[test]
    fn dry_run_summary_counts_only_refs_that_would_change() {
        use grit_lib::remote::transfer::UpdateMode;

        let raw = lib_outcome(&[("main", UpdateMode::UpToDate), ("x", UpdateMode::New)]);
        let remote = super::remote_outcome("origin".into(), &raw, true);
        assert_eq!(remote.updated, 0, "a dry run writes nothing");
        assert_eq!(remote.changed, 1);
        assert_eq!(remote.updates.len(), 1, "up-to-date refs are not listed");
        assert!(!remote.rejected);

        let (human, markdown) = render(&remote.as_single(true));
        assert!(
            human.contains("Would fetch 1 update from origin."),
            "{human}"
        );
        assert!(
            markdown.contains("Would fetch **1** update from `origin`."),
            "{markdown}"
        );
    }

    #[test]
    fn live_fetch_counts_changed_refs_and_rejections() {
        use super::FetchUpdateStatus;
        use grit_lib::remote::transfer::UpdateMode;

        let raw = lib_outcome(&[("main", UpdateMode::New), ("ff", UpdateMode::FastForward)]);
        let remote = super::remote_outcome("origin".into(), &raw, false);
        assert_eq!((remote.updated, remote.changed), (2, 2));
        let (human, markdown) = render(&remote.as_single(false));
        assert!(human.contains("Fetched 2 updates from origin."), "{human}");
        assert!(markdown.contains("Fetched **2** updates."), "{markdown}");

        let raw = lib_outcome(&[
            ("main", UpdateMode::New),
            ("lock", UpdateMode::RefLockRejected),
            ("gone", UpdateMode::SourceObjectNotFound),
        ]);
        let remote = super::remote_outcome("origin".into(), &raw, false);
        assert!(remote.rejected);
        assert_eq!(remote.changed, 1);
        let statuses: Vec<_> = remote.updates.iter().map(|u| u.status).collect();
        assert_eq!(
            statuses,
            [
                FetchUpdateStatus::New,
                FetchUpdateStatus::Rejected,
                FetchUpdateStatus::Rejected
            ]
        );
        let (human, markdown) = render(&remote.as_single(false));
        assert!(
            human.contains("Some ref updates from origin were rejected."),
            "{human}"
        );
        assert!(markdown.contains("were **rejected**"), "{markdown}");
    }

    #[test]
    fn fetch_update_status_serializes_snake_case() {
        use super::{FetchUpdate, FetchUpdateStatus};
        use grit_lib::progress::CancelToken;
        for (status, expected) in [
            (FetchUpdateStatus::New, "new"),
            (FetchUpdateStatus::FastForward, "fast_forward"),
            (FetchUpdateStatus::Rejected, "rejected"),
        ] {
            let update = FetchUpdate {
                ref_name: "refs/heads/main".to_owned(),
                remote_ref: "refs/heads/main".to_owned(),
                status,
                old_oid: None,
                new_oid: None,
                reason: None,
            };
            let value = serde_json::to_value(&update).expect("json");
            assert_eq!(value["status"].as_str(), Some(expected));
        }
    }
}

//! `grit fetch` — download refs and objects from a remote (default: the current
//! branch's remote, else `origin`).

use crate::commands::fetch_args::FetchArgs;
use crate::output::OutputMode;
use crate::progress::summary::{stdout_color, transfer_header, TransferSummary};
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::progress::Cancel;
use grit_lib::remote::fetch_request::{resolve_fetch_invocation, resolve_remote_argument};
use grit_lib::remote::transfer::{
    resolve_fetch_prune, resolve_fetch_tags, FetchDepth, FetchOptions,
    FetchOutcome as LibFetchOutcome, TagMode, TransferStats, UpdateMode,
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
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            Self::All(o) => o.remotes.iter().any(|r| {
                r.error.is_some()
                    || r.updates
                        .iter()
                        .any(|u| u.status == FetchUpdateStatus::Rejected)
            }),
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
            let one = FetchOutcome {
                remote: remote.remote.clone(),
                updates: remote.updates.clone(),
                updated: remote.updated,
                dry_run: self.dry_run,
                rejected: remote
                    .updates
                    .iter()
                    .any(|u| u.status == FetchUpdateStatus::Rejected),
                shallow: None,
                promisor_remotes: Vec::new(),
                stored_fields: Vec::new(),
                promisor_warnings: Vec::new(),
                transfer: TransferSummary::default(),
            };
            one.render_human(w, ctx)?;
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
            let one = FetchOutcome {
                remote: remote.remote.clone(),
                updates: remote.updates.clone(),
                updated: remote.updated,
                dry_run: self.dry_run,
                rejected: remote
                    .updates
                    .iter()
                    .any(|u| u.status == FetchUpdateStatus::Rejected),
                shallow: None,
                promisor_remotes: Vec::new(),
                stored_fields: Vec::new(),
                promisor_warnings: Vec::new(),
                transfer: TransferSummary::default(),
            };
            one.render_markdown(w, ctx)?;
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

impl From<UpdateMode> for FetchUpdateStatus {
    fn from(mode: UpdateMode) -> Self {
        match mode {
            UpdateMode::New => Self::New,
            UpdateMode::FastForward => Self::FastForward,
            UpdateMode::Forced => Self::Forced,
            UpdateMode::DeletedMissing => Self::Pruned,
            UpdateMode::NonFastForwardRejected
            | UpdateMode::TagUpdateRejected
            | UpdateMode::RefLockRejected => Self::Rejected,
            UpdateMode::UpToDate | UpdateMode::NoChangeNeeded | UpdateMode::Unborn => {
                Self::UpToDate
            }
            UpdateMode::SourceObjectNotFound => Self::Rejected,
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
        let pending = pending_update_count(&self.updates);
        let summary_count = if self.dry_run { pending } else { self.updated };
        if self.rejected {
            outln!(w, "Some ref updates from {} were rejected.", self.remote);
        } else if summary_count == 0 {
            outln!(w, "Already up to date with {}.", self.remote);
        } else if self.dry_run {
            outln!(
                w,
                "Would fetch {} update{} from {}.",
                summary_count,
                plural(summary_count),
                self.remote
            );
        } else {
            outln!(
                w,
                "Fetched {} update{} from {}.",
                self.updated,
                plural(self.updated),
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
        markdown::print_fetch_updates(w, &self.remote, &self.updates, self.updated, self.dry_run)?;
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

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(args: &FetchArgs, mode: OutputMode) -> Result<FetchRunOutcome> {
    if args.all {
        return run_all(args, mode).map(FetchRunOutcome::All);
    }
    run_one(args, mode).map(FetchRunOutcome::One)
}

fn run_one(args: &FetchArgs, mode: OutputMode) -> Result<FetchOutcome> {
    let interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let head = resolve_head_for_repository(&repo).ok();
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
    let url = config.get(&format!("remote.{remote_name}.url"));
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
        &repo,
        &config,
        &invocation,
        tags,
        prune_enabled,
        args.dry_run,
        fetch_depth,
        args.force,
        &factory,
        &mut progress,
        &interrupt,
    ) {
        Ok(result) => result,
        Err(err) => {
            if auth::offer_reauth(&err, url.as_deref().unwrap_or_default())? {
                fetch_once(
                    &repo,
                    &config,
                    &invocation,
                    tags,
                    prune_enabled,
                    args.dry_run,
                    fetch_depth,
                    args.force,
                    &factory,
                    &mut progress,
                    &interrupt,
                )?
            } else {
                return Err(err);
            }
        }
    };

    let (updates, updated, rejected) = map_updates(&result, args.dry_run);
    let shallow = shallow_json(&result, fetch_depth.is_some());
    let transfer = TransferSummary::new(
        result.transfer,
        progress.phases().to_vec(),
        progress.elapsed(),
    );

    Ok(FetchOutcome {
        remote: remote_name,
        updates,
        updated,
        dry_run: args.dry_run,
        rejected,
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
    let interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
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
            &interrupt,
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
                let (updates, updated, _rejected) = map_updates(&outcome, args.dry_run);
                total_updated += updated;
                remotes.push(FetchAllRemoteOutcome {
                    remote: name,
                    updates,
                    updated,
                    error: None,
                });
            }
            Err(err) => {
                remotes.push(FetchAllRemoteOutcome {
                    remote: name,
                    updates: Vec::new(),
                    updated: 0,
                    error: Some(err.to_string()),
                });
            }
        }
    }
    Ok(FetchAllOutcome {
        remotes,
        updated: total_updated,
        dry_run: args.dry_run,
        transfer: TransferSummary::new(received, progress.phases().to_vec(), progress.elapsed()),
    })
}

/// Refs that would change on a live fetch (everything except `up_to_date`).
fn pending_update_count(updates: &[FetchUpdate]) -> usize {
    updates
        .iter()
        .filter(|u| u.status != FetchUpdateStatus::UpToDate)
        .count()
}

fn map_updates(outcome: &LibFetchOutcome, dry_run: bool) -> (Vec<FetchUpdate>, usize, bool) {
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
                status: FetchUpdateStatus::from(update.mode),
                old_oid: update.old_oid.as_ref().map(|o| o.to_hex()),
                new_oid: update.new_oid.as_ref().map(|o| o.to_hex()),
                reason: rejection_reason(update.mode, &update.note),
            }
        })
        .collect();
    let updated = if dry_run {
        0
    } else {
        outcome.changed_ref_count()
    };
    (updates, updated, outcome.has_rejections())
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
    cancel: &dyn Cancel,
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

/// Preserve typed [`grit_lib::error::Error::HashCollision`] for JSON output and give
/// failures with a known next step their hint; other library errors keep the pre-mapper
/// string form (no top-level [`grit_lib::error::Error`] downcast).
pub(crate) fn map_remote_error(err: RemoteError) -> anyhow::Error {
    if let Some(guided) = crate::error_hints::guide_remote(&err) {
        return guided;
    }
    match err {
        RemoteError::Library(grit_lib::error::Error::BundleMissingPrerequisites) => {
            anyhow::Error::new(crate::commands::bundle::MissingPrerequisites)
        }
        RemoteError::Library(grit_lib::error::Error::HashCollision { oid }) => {
            crate::json_error::map_lib_error(grit_lib::error::Error::HashCollision { oid })
        }
        RemoteError::Library(hook @ grit_lib::error::Error::Hook(_)) => anyhow::Error::new(hook),
        RemoteError::Library(lib) if lib.is_cancelled() => anyhow::Error::new(lib),
        RemoteError::Library(lib) if lib.is_remote_unreadable() => anyhow::Error::new(lib),
        RemoteError::Library(lib) => anyhow::Error::msg(RemoteError::Library(lib).to_string()),
        other => anyhow::Error::msg(other.to_string()),
    }
}

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
        let err = map_remote_error(RemoteError::Library(LibError::ObjectNotFound(
            "deadbeef".into(),
        )));
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
        let err = map_remote_error(RemoteError::Library(LibError::HashCollision { oid }));
        let payload = structured_json_payload(&err).expect("structured payload");
        assert_eq!(payload.kind, crate::error_kind::ErrorKind::HashCollision);
        assert_eq!(payload.oid.as_deref(), Some(oid.to_hex().as_str()));
    }

    #[test]
    fn pending_update_count_ignores_up_to_date() {
        use super::{pending_update_count, FetchUpdate, FetchUpdateStatus};

        let updates = vec![
            FetchUpdate {
                ref_name: "refs/remotes/origin/main".into(),
                remote_ref: "refs/heads/main".into(),
                status: FetchUpdateStatus::UpToDate,
                old_oid: None,
                new_oid: None,
                reason: None,
            },
            FetchUpdate {
                ref_name: "refs/remotes/origin/x".into(),
                remote_ref: "refs/heads/x".into(),
                status: FetchUpdateStatus::New,
                old_oid: None,
                new_oid: Some("abc".into()),
                reason: None,
            },
        ];
        assert_eq!(pending_update_count(&updates), 1);
    }

    #[test]
    fn map_updates_dry_run_leaves_updated_at_zero() {
        use grit_lib::objects::ObjectId;
        use grit_lib::remote::transfer::{RefUpdate, UpdateMode};

        let oid = ObjectId::from_hex("a8e620a111111111111111111111111111111111").expect("oid");
        let raw = grit_lib::remote::transfer::FetchOutcome {
            updates: vec![RefUpdate {
                remote_ref: "refs/heads/main".into(),
                local_ref: Some("refs/remotes/origin/main".into()),
                old_oid: None,
                new_oid: Some(oid),
                mode: UpdateMode::New,
                note: None,
            }],
            ..Default::default()
        };
        let (_, updated, _) = super::map_updates(&raw, true);
        assert_eq!(updated, 0);
        let (_, updated, _) = super::map_updates(&raw, false);
        assert_eq!(updated, 1);
    }

    #[test]
    fn fetch_update_status_serializes_snake_case() {
        use super::{FetchUpdate, FetchUpdateStatus};
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

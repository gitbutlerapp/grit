//! `grit fetch` — download refs and objects from a remote (default `origin`).

use crate::commands::fetch_args::FetchArgs;
use crate::output::OutputMode;
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::fetch_request::resolve_fetch_invocation;
use grit_lib::remote::transfer::{
    resolve_fetch_prune, resolve_fetch_tags, FetchOptions, RefUpdate, TagMode, UpdateMode,
};
use grit_lib::remote::{DefaultHttpClientFactory, RemoteError, DEFAULT_REMOTE};
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
}

/// Result of `grit fetch --all`.
#[derive(Serialize, JsonSchema)]
pub struct FetchAllOutcome {
    pub remotes: Vec<FetchAllRemoteOutcome>,
    pub updated: usize,
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
                dry_run: false,
                rejected: remote
                    .updates
                    .iter()
                    .any(|u| u.status == FetchUpdateStatus::Rejected),
            };
            one.render_human(w, ctx)?;
        }
        Ok(())
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
                dry_run: false,
                rejected: false,
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
            UpdateMode::NonFastForwardRejected | UpdateMode::TagUpdateRejected => Self::Rejected,
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
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            if update.remote_ref == update.ref_name {
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
        if self.updated == 0 && !self.rejected {
            outln!(w, "Already up to date with {}.", self.remote);
        } else if self.updated > 0 {
            outln!(
                w,
                "Fetched {} update{} from {}.",
                self.updated,
                plural(self.updated),
                self.remote
            );
        }
        Ok(())
    }
}

impl MarkdownRender for FetchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_fetch_updates(w, &self.remote, &self.updates, self.updated)?;
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
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let invocation = resolve_fetch_invocation(
        &config,
        args.remote.as_deref(),
        &args.refspecs,
        DEFAULT_REMOTE,
    );
    let remote_name = invocation.remote.clone();
    let prune_enabled = resolve_fetch_prune(&config, &remote_name, args.prune_override())
        .context("could not resolve fetch prune settings")?;
    let tags = resolve_fetch_tags(&config, &remote_name, args.tag_mode_override());

    let factory = DefaultHttpClientFactory;
    let progress = if args.quiet {
        TerminalProgress::quiet(mode)
    } else {
        TerminalProgress::new(mode)
    };
    let mut progress = progress;
    let result = match fetch_once(
        &repo,
        &config,
        &invocation,
        tags,
        prune_enabled,
        args.dry_run,
        &factory,
        &mut progress,
    ) {
        Ok(result) => result,
        Err(err) => {
            let url = config
                .get(&format!("remote.{remote_name}.url"))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                fetch_once(
                    &repo,
                    &config,
                    &invocation,
                    tags,
                    prune_enabled,
                    args.dry_run,
                    &factory,
                    &mut progress,
                )?
            } else {
                return Err(err);
            }
        }
    };

    let (updates, updated, rejected) = map_updates(&result.updates);

    Ok(FetchOutcome {
        remote: remote_name,
        updates,
        updated,
        dry_run: args.dry_run,
        rejected,
    })
}

fn run_all(args: &FetchArgs, mode: OutputMode) -> Result<FetchAllOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let factory = DefaultHttpClientFactory;
    let progress = if args.quiet {
        TerminalProgress::quiet(mode)
    } else {
        TerminalProgress::new(mode)
    };
    let mut progress = progress;
    let base = FetchOptions {
        dry_run: args.dry_run,
        thread_policy: repo.thread_policy(),
        environment: crate::context::environment(),
        ..Default::default()
    };
    let raw = repo
        .remotes()
        .fetch_all(
            &config,
            &base,
            args.prune_override(),
            args.tag_mode_override(),
            &mut progress,
            Some(&factory),
        )
        .context("could not fetch all remotes")?;

    let mut remotes = Vec::new();
    let mut total_updated = 0usize;
    for (name, result) in raw {
        match result {
            Ok(outcome) => {
                let (updates, updated, _rejected) = map_updates(&outcome.updates);
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
    })
}

fn map_updates(raw: &[RefUpdate]) -> (Vec<FetchUpdate>, usize, bool) {
    let mut updates = Vec::new();
    let mut updated = 0usize;
    let mut rejected = false;
    for update in raw {
        if !include_in_report(update.mode) {
            continue;
        }
        let status = FetchUpdateStatus::from(update.mode);
        if update.mode.modified_repo() {
            updated += 1;
        }
        if status == FetchUpdateStatus::Rejected {
            rejected = true;
        }
        let ref_name = update
            .local_ref
            .clone()
            .unwrap_or_else(|| update.remote_ref.clone());
        updates.push(FetchUpdate {
            ref_name,
            remote_ref: update.remote_ref.clone(),
            status,
            old_oid: update.old_oid.as_ref().map(|o| o.to_hex()),
            new_oid: update.new_oid.as_ref().map(|o| o.to_hex()),
            reason: rejection_reason(update.mode, &update.note),
        });
    }
    (updates, updated, rejected)
}

fn include_in_report(mode: UpdateMode) -> bool {
    matches!(
        mode,
        UpdateMode::New
            | UpdateMode::FastForward
            | UpdateMode::Forced
            | UpdateMode::DeletedMissing
            | UpdateMode::NonFastForwardRejected
            | UpdateMode::TagUpdateRejected
            | UpdateMode::SourceObjectNotFound
    )
}

fn rejection_reason(mode: UpdateMode, note: &Option<String>) -> Option<String> {
    if note.is_some() && mode == UpdateMode::NonFastForwardRejected {
        return note.clone();
    }
    match mode {
        UpdateMode::NonFastForwardRejected => {
            Some("not a fast-forward — use a leading `+` on the refspec to force".to_owned())
        }
        UpdateMode::TagUpdateRejected => {
            Some("tag already exists locally and did not move".to_owned())
        }
        UpdateMode::SourceObjectNotFound => Some("source ref not found on the remote".to_owned()),
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
    factory: &DefaultHttpClientFactory,
    progress: &mut TerminalProgress,
) -> Result<grit_lib::remote::transfer::FetchOutcome> {
    let remote = crate::commands::remote::resolve_remote_or_url(Some(config), &invocation.remote)?;
    remote
        .fetch(
            repo,
            FetchOptions {
                refspecs: invocation.user_refspecs.clone(),
                tags,
                thread_policy: repo.thread_policy(),
                environment: crate::context::environment(),
                prune,
                dry_run,
                ..Default::default()
            },
            progress,
            Some(factory),
        )
        .map_err(map_remote_fetch_error)
}

/// Preserve typed [`grit_lib::error::Error::HashCollision`] for JSON output; other library
/// errors keep the pre-mapper string form (no top-level [`grit_lib::error::Error`] downcast).
pub(crate) fn map_remote_error(err: RemoteError) -> anyhow::Error {
    match err {
        RemoteError::Library(grit_lib::error::Error::BundleMissingPrerequisites) => {
            anyhow::Error::new(crate::commands::bundle::MissingPrerequisites)
        }
        RemoteError::Library(grit_lib::error::Error::HashCollision { oid }) => {
            crate::json_error::map_lib_error(grit_lib::error::Error::HashCollision { oid })
        }
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
        assert_eq!(payload.kind, "hash_collision");
        assert_eq!(payload.oid.as_deref(), Some(oid.to_hex().as_str()));
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

//! `grit clone` — copy a remote repository into a new directory.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use grit_lib::progress::CancelToken;
use grit_lib::remote::bundle_uri::BundleSeed;
use grit_lib::remote::clone::{
    cleanup_failed_clone, clone as clone_repo, clone_effective_single_branch, derive_clone_dir,
    dest_preexisted_empty, CloneCheckout, CloneError, CloneHead, CloneOptions, CloneTags,
};
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transport_path::redact_url_for_display;

use crate::progress::summary::TransferSummary;
use crate::progress::{paint, TerminalProgress, BOLD, DIM};
pub use crate::render::clone::{
    CloneBlobsJson, CloneBundleFailure, CloneBundleJson, CloneNoCheckout, CloneOutcome, ClonePhase,
};
use crate::render::promisor;
use grit_lib::progress::{ProgressPhase, ProgressUnit};

/// Usage mistakes specific to `grit clone` (exit code 2).
#[derive(Debug)]
pub struct CloneUsageError;

impl std::fmt::Display for CloneUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid clone arguments")
    }
}

impl std::error::Error for CloneUsageError {}

impl CloneBlobsJson {
    /// `None` for filter kinds `--blobs` cannot express.
    fn from_spec(spec: &ObjectFilterSpec) -> Option<Self> {
        match spec {
            ObjectFilterSpec::BlobNone => Some(Self::None),
            ObjectFilterSpec::BlobLimit(n) => Some(Self::Limit(*n)),
            _ => None,
        }
    }
}

/// Options for [`run`].
pub struct CloneRunOptions {
    pub url: String,
    pub dir: Option<String>,
    pub depth: Option<u32>,
    pub branch: Option<String>,
    pub shallow: bool,
    pub single_branch: Option<bool>,
    pub no_tags: bool,
    pub origin: Option<String>,
    pub bare: bool,
    pub no_checkout: bool,
    pub blobs: Option<String>,
    /// `--config KEY=VALUE` entries written into the new repository.
    pub config: Vec<grit_lib::config::environment::ConfigOverride>,
    pub quiet: crate::quiet_args::QuietArgs,
}

fn phase_json_name(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::ReceivingObjects => "receiving",
        ProgressPhase::IndexingObjects => "indexing",
        ProgressPhase::ResolvingDeltas => "resolving_deltas",
        ProgressPhase::CopyingObjects => "copying",
        ProgressPhase::CheckingOutFiles => "checkout",
        ProgressPhase::TransferringBundle => "bundle",
        ProgressPhase::StagingFiles => "staging",
        _ => "other",
    }
}

fn unit_json_name(unit: ProgressUnit) -> &'static str {
    match unit {
        ProgressUnit::Bytes => "bytes",
        ProgressUnit::Objects => "objects",
        ProgressUnit::Files => "files",
        _ => "items",
    }
}

fn tag_fetch_mode_label(mode: CloneTags) -> &'static str {
    match mode {
        CloneTags::All => "all",
        CloneTags::Following => "following",
        CloneTags::None => "none",
    }
}

fn resolved_clone_tags(opts: &CloneRunOptions) -> Option<CloneTags> {
    if opts.no_tags {
        Some(CloneTags::None)
    } else {
        None
    }
}

fn effective_tag_fetch_mode(opts: &CloneRunOptions) -> CloneTags {
    resolved_clone_tags(opts).unwrap_or_else(|| {
        let clone_opts = CloneOptions {
            depth: opts.depth.or_else(|| opts.shallow.then_some(1)),
            single_branch: opts.single_branch,
            ..CloneOptions::new(String::new(), std::path::PathBuf::new())
        };
        if clone_effective_single_branch(&clone_opts) {
            CloneTags::Following
        } else {
            CloneTags::All
        }
    })
}

/// A parsed `--blobs` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobsArg {
    /// `none` or a size.
    Filter(ObjectFilterSpec),
    /// `auto`: take the filter from the accepted promisor remotes.
    Auto,
}

fn parse_blobs_filter(raw: Option<&str>) -> Result<Option<BlobsArg>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.trim().eq_ignore_ascii_case("auto") {
        return Ok(Some(BlobsArg::Auto));
    }
    ObjectFilterSpec::from_blobs_cli(raw)
        .map(|spec| Some(BlobsArg::Filter(spec)))
        .map_err(|e| {
            anyhow::Error::from(CloneUsageError).context(format!("invalid --blobs value: {e}"))
        })
}

/// Clone `opts.url`; `human_interactive` says whether stderr is for people (not `--json` or `--markdown`).
///
/// Notes gathered along the way are on the returned outcome; call
/// [`CloneOutcome::render_notes`] to show them.
pub fn run(mut opts: CloneRunOptions, human_interactive: bool) -> Result<CloneOutcome> {
    let url = &opts.url;
    let filter = parse_blobs_filter(opts.blobs.as_deref())?;
    opts.depth = opts.depth.or_else(|| opts.shallow.then_some(1));
    if matches!(opts.depth, Some(0)) {
        return Err(anyhow::anyhow!("depth 0 is not a positive number"));
    }
    let dir = match &opts.dir {
        Some(d) => d.clone(),
        None => derive_clone_dir(url, opts.bare).map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let path = PathBuf::from(&dir);
    let display_url = redact_url_for_display(url);
    let dest_preexisted_empty = dest_preexisted_empty(&path);
    let started = Instant::now();
    // Cleanup must wait until the library has stopped writing, so Ctrl+C only
    // cancels; the error arm below then removes the half-made clone.
    let _interrupt = crate::interrupt::InterruptCancel::install();

    // A clone always ends with a checkout, so the overall bar can size it.
    let mut term_progress = opts.quiet.progress(human_interactive).with_overall(true);
    if term_progress.is_drawing() {
        let color = term_progress.uses_color();
        eprintln!(
            "{} {} {} {}",
            paint(color, BOLD, "Cloning"),
            paint(color, DIM, &display_url),
            paint(color, DIM, "→"),
            paint(color, BOLD, &dir)
        );
    } else if term_progress.reports_status() {
        eprintln!("Cloning into '{dir}' ...");
    }
    match clone_into(
        &opts,
        filter,
        &path,
        &dir,
        &display_url,
        started,
        &mut term_progress,
        &crate::interrupt::process_cancel_token(),
    ) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let skip_cleanup = matches!(
                err.downcast_ref::<grit_lib::error::Error>(),
                Some(e) if !grit_lib::remote::clone::should_cleanup_failed_clone(e)
            ) || matches!(
                err.downcast_ref::<CloneError>(),
                Some(e) if !e.should_cleanup_failed_clone()
            );
            if !skip_cleanup {
                cleanup_failed_clone(&path, dest_preexisted_empty);
            }
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn clone_into(
    run: &CloneRunOptions,
    blobs: Option<BlobsArg>,
    path: &Path,
    dir: &str,
    display_url: &str,
    started: Instant,
    term_progress: &mut TerminalProgress,
    cancel: &CancelToken,
) -> Result<CloneOutcome> {
    let factory = crate::context::http_client_factory();
    let remote_name = run
        .origin
        .clone()
        .unwrap_or_else(|| grit_lib::remote::DEFAULT_REMOTE.to_owned());
    let tag_mode = effective_tag_fetch_mode(run);
    let filter = match blobs {
        Some(BlobsArg::Filter(spec)) => Some(spec),
        Some(BlobsArg::Auto) | None => None,
    };
    let auto_filter = blobs == Some(BlobsArg::Auto);
    let clone_opts = CloneOptions {
        url: run.url.clone(),
        dest: path.to_path_buf(),
        environment: crate::context::environment(),
        remote_name: remote_name.clone(),
        initial_branch: "main".to_owned(),
        thread_policy: crate::context::thread_policy_for_repo(),
        depth: run.depth,
        branch: run.branch.clone(),
        filter,
        auto_filter,
        config: run
            .config
            .iter()
            .map(|c| (c.key().to_owned(), c.value().unwrap_or("true").to_owned()))
            .collect(),
        single_branch: run.single_branch,
        tags: resolved_clone_tags(run),
        promisor_fetcher: Some(std::sync::Arc::new(
            grit_lib::objects::promisor::DefaultPromisorFetcher::new(std::sync::Arc::new(
                factory.clone(),
            )),
        )),
        bare: run.bare,
        checkout: !run.no_checkout,
        bundle_cache: bundle_cache_dir(),
    };
    let single_branch = clone_effective_single_branch(&clone_opts);
    let outcome = clone_repo(clone_opts, &mut *term_progress, cancel, Some(&factory))?;
    let mut bundle_failure = None;
    let bundle = match &outcome.bundle {
        BundleSeed::Seeded(seeded) => Some(CloneBundleJson {
            uri: redact_url_for_display(&seeded.uri),
            bytes: seeded.bytes,
            resumed_from: seeded.resumed_from,
        }),
        BundleSeed::Failed { uri, reason, .. } => {
            bundle_failure = Some(CloneBundleFailure {
                uri: uri.as_deref().map(redact_url_for_display),
                reason: reason.to_string(),
            });
            None
        }
        BundleSeed::NotOffered => None,
        _ => None,
    };

    let files = outcome.files;
    let worktree_bytes = outcome.worktree_bytes;
    let path_collisions = outcome.path_collisions.clone();
    let submodule_paths = outcome.submodule_paths.clone();
    let bare_gitlink_paths = outcome.bare_gitlink_paths.clone();
    let (no_checkout_reason, branches_with_commits, no_checkout_note) = match outcome.checkout {
        CloneCheckout::CheckedOut(_) => (None, None, None),
        CloneCheckout::Skipped => {
            let reason = if run.bare {
                Some("bare".to_owned())
            } else {
                Some("no_checkout".to_owned())
            };
            (reason, None, None)
        }
        CloneCheckout::EmptyRemote | CloneCheckout::UnbornDefaultBranch => {
            let (reason, note) = match outcome.checkout {
                CloneCheckout::EmptyRemote => ("empty_remote", CloneNoCheckout::EmptyRemote),
                _ => (
                    "unborn_default_branch",
                    CloneNoCheckout::UnbornDefaultBranch,
                ),
            };
            let tips = (!outcome.branches_with_commits.is_empty())
                .then(|| outcome.branches_with_commits.clone());
            (Some(reason.to_owned()), tips, Some(note))
        }
        // Checkout states this CLI does not know yet: report no reason.
        _ => (None, None, None),
    };
    let elapsed = started.elapsed();
    let transfer = TransferSummary::new(
        outcome.transfer.clone(),
        term_progress.phases().to_vec(),
        elapsed,
    )
    .with_milestones(term_progress.milestones().to_vec());

    let (branch, detached, tag) = match &outcome.head {
        CloneHead::Branch(name) => (Some(name.clone()), false, None),
        CloneHead::Detached { tag, .. } => (None, true, Some(tag.clone())),
        _ => (None, false, None),
    };

    let space = if outcome.bare {
        None
    } else {
        space_for_clone(dir, &run.url, &remote_name)
    };
    Ok(CloneOutcome {
        url: display_url.to_owned(),
        path: dir.to_owned(),
        remote: remote_name,
        single_branch,
        tag_fetch: tag_fetch_mode_label(tag_mode).to_owned(),
        branch,
        detached,
        tag,
        depth: run.depth,
        blobs: outcome.filter.as_ref().and_then(CloneBlobsJson::from_spec),
        partial: outcome.filter_sent,
        filter_auto: auto_filter,
        filter_ignored: outcome.filter_ignored(),
        promisor_remotes: promisor::promisor_remotes(outcome.promisor.as_ref()),
        stored_fields: promisor::stored_fields(outcome.promisor.as_ref()),
        promisor_warnings: promisor::warnings(outcome.promisor.as_ref()),
        objects: outcome.transfer.objects,
        bytes: outcome.transfer.bytes,
        branches: outcome.branches,
        tags: outcome.tags,
        files,
        object_types: transfer.object_types,
        worktree_bytes,
        seconds: elapsed.as_secs_f64(),
        phases: transfer
            .records
            .iter()
            .map(|r| ClonePhase {
                phase: phase_json_name(r.phase).to_owned(),
                unit: unit_json_name(r.unit).to_owned(),
                count: r.count,
                seconds: r.elapsed.as_secs_f64(),
            })
            .collect(),
        transfer,
        no_checkout_reason,
        branches_with_commits,
        path_collisions,
        submodule_paths,
        bare_gitlink_paths,
        bare: outcome.bare,
        bundle,
        space,
        bundle_failure,
        no_checkout_note,
    })
}

/// For a clone of an organization's project on the default grit host
/// (GitButler Code): set the clone up to push to your space
/// ([`crate::commands::sync::ensure_space`]) and name it. Best effort: when
/// you aren't signed in, or anything else fails, the clone stays as it is and
/// `grit sync` sets it up later.
fn space_for_clone(dir: &str, url: &str, remote: &str) -> Option<String> {
    use grit_lib::remote::grit_server_auth::ServerUrl;
    use grit_lib::remote::grit_server_space::hosted_project;
    let host = ServerUrl::parse(&crate::commands::auth::default_grit_host_url()).ok()?;
    let (server, _) = hosted_project(url)?;
    if server != host {
        return None;
    }
    let (options, _sink) = crate::diagnostics::repository_options(false);
    let repo =
        grit_lib::repo::Repository::discover(Some(std::path::Path::new(dir)), &options).ok()?;
    let env = crate::context::environment();
    let config = grit_lib::config::ConfigSet::load(
        &env,
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(&env, Some(repo.git_dir())),
    )
    .ok()?;
    let setup = crate::commands::sync::ensure_space(&repo, &config, remote).ok()??;
    // Shared branches: bring the other members' heads now, so the first
    // `grit status` shows them.
    if !setup.space.shared.is_empty() {
        let _ = crate::commands::sync::fetch_shared_heads(&repo, &setup.remote, false);
    }
    (setup.remote != remote).then_some(setup.space.project)
}

/// Where clone keeps bundle downloads between attempts:
/// `$XDG_CACHE_HOME/grit/bundles`, else `~/.cache/grit/bundles`.
fn bundle_cache_dir() -> Option<PathBuf> {
    let env = crate::context::environment();
    let base = env
        .var("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.var("HOME")
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("grit").join("bundles"))
}

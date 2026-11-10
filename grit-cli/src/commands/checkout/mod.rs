//! `grit checkout <url>` — start work on a repository from a snapshot of its
//! target branch instead of a clone.

use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::short_hex;
use grit_lib::remote::resumable::{
    claim_resumable_download, discard_download, download_resumable, ResumableDownloadDest,
};
use grit_lib::remote::snapshot_checkout::{
    materialize, normalize_paths, suggest_work_branch, BranchInfo, BranchRefresh,
    BranchRefreshResult, CheckoutClient, CheckoutMarker, CheckoutRequest, CheckoutState,
    MaterializeRequest, RefreshOutcome,
};
use grit_lib::remote::{redact_url_for_display, HttpClientFactory};

use crate::dispatch::CommandInteraction;
use crate::progress::{paint, TerminalProgress, BOLD, DIM};
pub use crate::render::pull::{
    PullRefreshJson, PullServerBranchJson, RefreshBehindJson, RefreshChangesJson,
    RefreshFollowedJson,
};
use crate::ui::filesystem_path_for_user;

pub use crate::render::checkout::*;

fn download_dir() -> Option<PathBuf> {
    let env = crate::context::environment();
    let base = env
        .var("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.var("HOME")
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("grit").join("checkouts"))
}

/// Two random bytes for a work branch's name.
pub(crate) fn random_pair() -> [u8; 2] {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0);
    let n = hasher.finish();
    [n as u8, (n >> 8) as u8]
}

/// `grit checkout <url> [<slice>] [--path <path>]... [--into <dir>]`.
pub fn run(
    url: &str,
    slice: Option<&str>,
    paths: &[String],
    into: Option<&Path>,
    interaction: CommandInteraction,
) -> Result<CheckoutOutcome> {
    let started = Instant::now();
    let env = crate::context::environment();
    let config = ConfigSet::load(
        &env,
        None,
        &grit_lib::config::LoadConfigOptions::standard(&env, None),
    )
    .context("could not load config")?;
    let http = crate::context::http_client_factory()
        .create(&config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = CheckoutClient::new(http.as_ref(), url)?;
    let server = client.server_info()?;
    let mut wanted = Vec::new();
    if let Some(name) = slice {
        let slices = client.slices()?;
        let Some(found) = slices.slices.iter().find(|s| s.name == name) else {
            let names: Vec<&str> = slices.slices.iter().map(|s| s.name.as_str()).collect();
            anyhow::bail!(
                "{} has no slice {name}; {}",
                redact_url_for_display(url),
                if names.is_empty() {
                    "it has none yet (make one with `grit slice new <name> --path <dir>`)"
                        .to_owned()
                } else {
                    format!("it has {}", names.join(", "))
                }
            );
        };
        wanted.extend(found.paths.iter().cloned());
    }
    wanted.extend(paths.iter().cloned());
    let paths = normalize_paths(&wanted)?;
    let dest = match into {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir()?.join(slice.unwrap_or(&server.name)),
    };
    if dest.is_dir() && std::fs::read_dir(&dest)?.next().is_some() {
        anyhow::bail!(
            "{} already exists and is not empty",
            filesystem_path_for_user(&dest)
        );
    }
    let mut progress = TerminalProgress::new(interaction.human_progress);
    if progress.is_drawing() {
        let color = progress.uses_color();
        let what = if paths.is_empty() {
            "the whole tree".to_owned()
        } else {
            paths.join(", ")
        };
        eprintln!(
            "{} {} {} {} {}",
            paint(color, BOLD, "Checking out"),
            paint(color, DIM, &what),
            paint(
                color,
                DIM,
                &format!("of {} at {}", server.branch, short_hex(&server.head))
            ),
            paint(color, DIM, "→"),
            paint(color, BOLD, &filesystem_path_for_user(&dest)),
        );
    }

    let email = crate::context::config_string(&config, "user.email");
    let request = CheckoutRequest {
        paths: paths.clone(),
        client: dest.file_name().map(|n| n.to_string_lossy().into_owned()),
        work: Some(suggest_work_branch(email.as_deref(), random_pair())),
    };
    let info = client.create(&request)?;
    let work = info
        .work
        .clone()
        .or(request.work.clone())
        .unwrap_or_else(|| "work".to_owned());

    let cache = download_dir().unwrap_or_else(|| std::env::temp_dir().join("grit-checkouts"));
    let shared_bundle = cache.join(format!("{}.bundle", info.root));
    let slot = claim_resumable_download(&cache, &shared_bundle)?;
    let (bundle, _lock) = match slot {
        ResumableDownloadDest::Shared { dest, lock } => (dest, Some(lock)),
        ResumableDownloadDest::Private(dest) => (dest, None),
        _ => anyhow::bail!("unsupported download destination for the checkout bundle"),
    };
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let downloaded = download_resumable(
        http.as_ref(),
        &client.bundle_url(&info.id),
        &bundle,
        &mut progress,
        &crate::interrupt::process_cancel_token(),
    )?;

    let existed = dest.exists();
    let made = materialize(
        &MaterializeRequest {
            dest: &dest,
            info: &info,
            bundle: &bundle,
            repo_url: &client.url().repo_url,
            work: &work,
        },
        &mut progress,
        &crate::interrupt::process_cancel_token(),
    );
    if let Err(e) = made {
        if !existed {
            let _ = std::fs::remove_dir_all(&dest);
        }
        return Err(e.into());
    }
    let _ = discard_download(&bundle);
    drop(progress);

    Ok(CheckoutOutcome {
        url: redact_url_for_display(&client.url().repo_url),
        path: filesystem_path_for_user(&dest),
        branch: info.branch,
        work,
        base: info.base,
        snapshot: info.root,
        paths: info.paths,
        files: info.files,
        bytes: info.bytes,
        download_bytes: downloaded.bytes,
        resumed_from: downloaded.resumed_from,
        id: info.id,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// A snapshot checkout opened from the current directory.
pub(crate) struct OpenCheckout {
    pub(crate) repo: grit_lib::repo::Repository,
    pub(crate) marker: CheckoutMarker,
    pub(crate) config: ConfigSet,
    pub(crate) http: Box<dyn grit_lib::remote::http_client::HttpClient>,
}

impl OpenCheckout {
    /// The client for the server the checkout came from.
    pub(crate) fn client(&self) -> Result<CheckoutClient<'_>> {
        Ok(CheckoutClient::new(self.http.as_ref(), &self.marker.url)?)
    }

    /// Who server-made commits are by ([`author_for`]).
    pub(crate) fn author(&self) -> Option<grit_lib::remote::snapshot_checkout::Ident> {
        author_for(&self.config)
    }
}

/// Who server-made commits (an integration's merge, a slice change) are by:
/// the author the user's own commits get, from `GIT_AUTHOR_NAME` and
/// `GIT_AUTHOR_EMAIL`, then `author.*` and `user.*` in `config`. `None` when
/// none is configured, so the server uses the signed-in user.
pub(crate) fn author_for(config: &ConfigSet) -> Option<grit_lib::remote::snapshot_checkout::Ident> {
    grit_lib::remote::snapshot_checkout::Ident::author(&crate::context::environment(), config).ok()
}

/// The checkout the current directory is in.
///
/// # Returns
///
/// `None` when the current repository is not a snapshot checkout.
pub(crate) fn open_checkout_here() -> Result<Option<OpenCheckout>> {
    let repo = crate::context::discover()?;
    open_checkout(repo)
}

/// `repo` as a snapshot checkout, when it is one.
pub(crate) fn open_checkout(repo: grit_lib::repo::Repository) -> Result<Option<OpenCheckout>> {
    let Some(marker) = CheckoutMarker::read(repo.git_dir()) else {
        return Ok(None);
    };
    let config = ConfigSet::load(
        &crate::context::environment(),
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(
            &crate::context::environment(),
            Some(repo.git_dir()),
        ),
    )
    .context("could not load config")?;
    let http = crate::context::http_client_factory()
        .create(&config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(OpenCheckout {
        repo,
        marker,
        config,
        http,
    }))
}

/// [`open_checkout_here`], failing outside a checkout.
pub(crate) fn require_checkout() -> Result<OpenCheckout> {
    open_checkout_here()?.ok_or_else(|| {
        anyhow::anyhow!("this is not a snapshot checkout; make one with `grit checkout <url>`")
    })
}

impl PullRefreshJson {
    /// The JSON of a refresh the library did.
    #[must_use]
    pub fn from_outcome(r: &RefreshOutcome) -> Self {
        Self {
            target: r.target.clone(),
            upstream_ref: r.upstream_ref.clone(),
            base: r.current.base.clone(),
            snapshot: r.current.commit.clone(),
            previous_base: r.previous.as_ref().map(|p| p.base.clone()),
            previous_snapshot: r.previous.as_ref().map(|p| p.commit.clone()),
            moved: r.moved,
            changes: r.changes.map(|c| RefreshChangesJson {
                files: c.files_changed,
                insertions: c.insertions,
                deletions: c.deletions,
            }),
            dry_run: false,
            followed: r
                .followed
                .iter()
                .map(|f| RefreshFollowedJson {
                    name: f.name.clone(),
                    from: f.from.to_hex(),
                    to: f.to.to_hex(),
                })
                .collect(),
            behind: r
                .behind
                .iter()
                .map(|b| RefreshBehindJson {
                    name: b.name.clone(),
                    tip: b.tip.to_hex(),
                })
                .collect(),
            gave_change_ids: r.gave_change_ids,
        }
    }

    /// The JSON of `grit pull --dry-run`: the checkout as `marker` has it, and
    /// where the server says its target is now.
    #[must_use]
    pub fn dry_run(marker: &CheckoutMarker, state: &CheckoutState) -> Self {
        let last = marker.last();
        let previous_base = last.map(|r| r.base.clone());
        let base = state
            .tip
            .clone()
            .or_else(|| previous_base.clone())
            .unwrap_or_default();
        Self {
            target: marker.branch.clone(),
            upstream_ref: marker.upstream_ref(),
            moved: previous_base.as_deref() != Some(base.as_str()),
            base,
            snapshot: last.map(|r| r.commit.clone()).unwrap_or_default(),
            previous_snapshot: last.map(|r| r.commit.clone()),
            previous_base,
            changes: None,
            dry_run: true,
            followed: Vec::new(),
            behind: Vec::new(),
            gave_change_ids: 0,
        }
    }
}

impl PullServerBranchJson {
    /// The JSON of a `grit pull <branch>` the library did.
    #[must_use]
    pub fn from_refresh(info: &BranchInfo, r: &BranchRefresh) -> Self {
        Self {
            name: r.name.clone(),
            upstream_ref: r.upstream_ref.clone(),
            tip: info.tip.clone(),
            projected: r.projected.to_hex(),
            commits: info.commits,
            made: info.made,
            reused: info.reused,
            previous: r.previous.map(|p| p.to_hex()),
            own: match r.result {
                BranchRefreshResult::Rebased { own } => own,
                _ => 0,
            },
            switched: r.switched,
            was_on: r.was_on.clone(),
            gave_change_ids: r.gave_change_ids,
        }
    }
}

mod dispatch;
pub use dispatch::dispatch;

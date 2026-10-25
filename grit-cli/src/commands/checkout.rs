//! `grit checkout <url>` — start work on a repository from a snapshot of its
//! target branch instead of a clone.

use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::resumable::{discard_download, download_resumable};
use grit_lib::remote::snapshot_checkout::{
    materialize, normalize_paths, suggest_work_branch, CheckoutClient, CheckoutMarker,
    CheckoutRequest, CheckoutState, RefreshOutcome,
};
use grit_lib::remote::HttpClientFactory;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::{format_bytes, format_duration, paint, TerminalProgress, BOLD, DIM};

/// Result of `grit checkout <url>`.
#[derive(Serialize, JsonSchema)]
pub struct CheckoutOutcome {
    /// The repository URL, configured as `origin`.
    pub url: String,
    /// Directory the checkout was made in.
    pub path: String,
    /// The server's target branch the snapshot was taken from.
    pub branch: String,
    /// The new branch checked out for your work.
    pub work: String,
    /// The real commit of the target branch the snapshot stands for.
    pub base: String,
    /// The snapshot commit (the checkout's only commit).
    pub snapshot: String,
    /// The paths checked out; empty for the whole tree.
    pub paths: Vec<String>,
    /// Files in the snapshot.
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Size of the download.
    pub download_bytes: u64,
    /// Bytes an earlier, interrupted checkout had already downloaded.
    pub resumed_from: u64,
    /// The server's id for the checkout.
    pub id: String,
    /// Wall-clock seconds.
    pub seconds: f64,
}

impl HumanRender for CheckoutOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = crate::progress::summary::stdout_color();
        let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
        outln!(
            w,
            "{} Checked out {} on {} in {}",
            paint(color, "32", "✓"),
            paint(color, BOLD, &self.path),
            paint(color, BOLD, &self.work),
            format_duration(std::time::Duration::from_secs_f64(self.seconds)),
        );
        outln!(w);
        let paths = if self.paths.is_empty() {
            "the whole tree".to_owned()
        } else {
            self.paths.join(", ")
        };
        outln!(w, "  {}  {paths}", paint(color, DIM, "paths   "));
        outln!(
            w,
            "  {}  {} files, {} as a {} download{}",
            paint(color, DIM, "files   "),
            self.files,
            format_bytes(self.bytes),
            format_bytes(self.download_bytes),
            if self.resumed_from > 0 {
                format!(" (resumed at {})", format_bytes(self.resumed_from))
            } else {
                String::new()
            }
        );
        outln!(
            w,
            "  {}  one commit, {}, standing for {} at {}",
            paint(color, DIM, "history "),
            short(&self.snapshot),
            self.branch,
            short(&self.base),
        );
        outln!(w);
        outln!(
            w,
            "Commit on {} as usual; origin/{} is the snapshot.",
            self.work,
            self.branch
        );
        Ok(())
    }
}

impl MarkdownRender for CheckoutOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Checkout complete");
        outln!(w);
        outln!(w, "- **path**: `{}`", self.path);
        outln!(w, "- **work branch**: `{}`", self.work);
        outln!(w, "- **url**: `{}`", self.url);
        outln!(w, "- **target**: `{}` at `{}`", self.branch, self.base);
        outln!(w, "- **snapshot**: `{}`", self.snapshot);
        if self.paths.is_empty() {
            outln!(w, "- **paths**: the whole tree");
        } else {
            let paths: Vec<String> = self.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(w, "- **paths**: {}", paths.join(", "));
        }
        outln!(w, "- **files**: {} ({} bytes)", self.files, self.bytes);
        outln!(
            w,
            "- **download**: {} bytes (resumed at {})",
            self.download_bytes,
            self.resumed_from
        );
        Ok(())
    }
}

/// Where checkout downloads are kept between attempts.
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

/// Two random bytes for the work branch's name.
fn random_pair() -> [u8; 2] {
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
    mode: OutputMode,
) -> Result<CheckoutOutcome> {
    let started = Instant::now();
    let env = crate::context::environment();
    let config = ConfigSet::load(&env, None, true).context("could not load config")?;
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
                "{url} has no slice {name}; {}",
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
        anyhow::bail!("{} already exists and is not empty", dest.display());
    }
    let mut progress = TerminalProgress::new(mode);
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
                &format!(
                    "of {} at {}",
                    server.branch,
                    server.head.get(..7).unwrap_or(&server.head)
                )
            ),
            paint(color, DIM, "→"),
            paint(color, BOLD, &dest.display().to_string()),
        );
    }

    let email = config.get("user.email");
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
    std::fs::create_dir_all(&cache)?;
    let bundle = cache.join(format!("{}.bundle", info.root));
    let downloaded = download_resumable(
        http.as_ref(),
        &client.bundle_url(&info.id),
        &bundle,
        &mut progress,
    )?;

    let existed = dest.exists();
    let made = materialize(
        &dest,
        &info,
        &bundle,
        &client.url().repo_url,
        &work,
        &mut progress,
        &NeverCancel,
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
        url: client.url().repo_url.clone(),
        path: dest.display().to_string(),
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

    /// Who server-made commits are by: `user.name` and `user.email`.
    pub(crate) fn author(&self) -> Option<grit_lib::remote::snapshot_checkout::Ident> {
        let email = self.config.get("user.email")?;
        let name = self
            .config
            .get("user.name")
            .unwrap_or_else(|| email.clone());
        Some(grit_lib::remote::snapshot_checkout::Ident { name, email })
    }
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
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
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

/// What `grit pull` refreshed in a checkout.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PullRefreshJson {
    /// The server's target branch.
    pub target: String,
    /// The local ref holding the newest snapshot, `refs/remotes/origin/<target>`.
    pub upstream_ref: String,
    /// The real commit the newest snapshot stands for. With `--dry-run`, where
    /// the target is now.
    pub base: String,
    /// The newest snapshot commit, now `origin/<target>`. With `--dry-run`, the
    /// checkout's snapshot as it is.
    pub snapshot: String,
    /// The real commit the checkout's previous snapshot stood for.
    pub previous_base: Option<String>,
    /// The checkout's previous snapshot.
    pub previous_snapshot: Option<String>,
    /// Whether the target had moved since the checkout's last snapshot.
    pub moved: bool,
    /// What changed in the checkout's paths between the two snapshots; null
    /// when the target didn't move.
    pub changes: Option<RefreshChangesJson>,
    /// With `--dry-run`: nothing was fetched or changed, and the server made
    /// no snapshot.
    pub dry_run: bool,
    /// The local branch named after the target, with no commits of its own,
    /// that moved to the new snapshot.
    pub followed: Vec<RefreshFollowedJson>,
    /// Other branches with commits of their own still on an older snapshot.
    pub behind: Vec<RefreshBehindJson>,
}

/// What changed in a checkout's paths between two snapshots, as
/// `git diff --shortstat` counts it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshChangesJson {
    /// Files changed.
    pub files: usize,
    /// Lines added.
    pub insertions: usize,
    /// Lines removed.
    pub deletions: usize,
}

/// A branch a refresh moved along with the snapshot.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshFollowedJson {
    /// The branch.
    pub name: String,
    /// The older snapshot it was at.
    pub from: String,
    /// The new snapshot it is at.
    pub to: String,
}

/// A branch still building on an older snapshot after a refresh.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshBehindJson {
    /// The branch.
    pub name: String,
    /// Its tip.
    pub tip: String,
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
        }
    }
}

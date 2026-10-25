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
    materialize, normalize_paths, suggest_work_branch, CheckoutClient, CheckoutRequest,
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

/// `grit checkout <url> [--path <path>]... [--into <dir>]`.
pub fn run(
    url: &str,
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
    let paths = normalize_paths(paths)?;
    let dest = match into {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir()?.join(&server.name),
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

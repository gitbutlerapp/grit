//! `grit clone` — copy a remote repository into a new directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grit_lib::clone::{
    cleanup_failed_clone, clone as clone_repo, derive_clone_dir, dest_preexisted_empty, CloneError,
    CloneOptions,
};
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::remote::DefaultHttpClientFactory;
use serde::Serialize;

use crate::context;
use crate::output::{outln, progress, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::{format_bytes, TerminalProgress};

/// Result of `grit clone`.
#[derive(Serialize)]
pub struct CloneOutcome {
    pub url: String,
    /// Destination directory.
    pub path: String,
    /// Default branch checked out.
    pub branch: String,
    /// Objects received during the fetch.
    pub objects: u64,
    /// Pack bytes received during the fetch.
    pub bytes: u64,
    /// Remote branches fetched.
    pub branches: u64,
    /// Remote tags fetched.
    pub tags: u64,
    /// Working-tree files checked out.
    pub files: u64,
    #[serde(skip)]
    pub elapsed: Duration,
}

impl HumanRender for CloneOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let size = format_bytes(self.bytes);
        let secs = self.elapsed.as_secs_f64();
        outln!(
            w,
            "Cloned into '{}' on branch {} — {} objects ({size}), {} branches, {} tags, {} files in {secs:.2}s.",
            self.path,
            self.branch,
            self.objects,
            self.branches,
            self.tags,
            self.files,
        );
        Ok(())
    }
}

impl MarkdownRender for CloneOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let size = format_bytes(self.bytes);
        let secs = self.elapsed.as_secs_f64();
        outln!(w, "## Clone complete");
        outln!(w);
        outln!(w, "- **path**: `{}`", self.path);
        outln!(w, "- **branch**: `{}`", self.branch);
        outln!(w, "- **url**: `{}`", self.url);
        outln!(w, "- **objects**: {}", self.objects);
        outln!(w, "- **bytes**: {} ({size})", self.bytes);
        outln!(w, "- **branches**: {}", self.branches);
        outln!(w, "- **tags**: {}", self.tags);
        outln!(w, "- **files**: {}", self.files);
        outln!(w, "- **elapsed**: {secs:.2}s");
        Ok(())
    }
}

pub fn run(url: &str, dir: Option<String>, mode: OutputMode) -> Result<CloneOutcome> {
    let dir = dir.unwrap_or_else(|| derive_clone_dir(url));
    let path = PathBuf::from(&dir);
    let dest_preexisted_empty = dest_preexisted_empty(&path);
    let started = Instant::now();

    progress(mode, &format!("Cloning into '{dir}' ..."));
    match clone_into(url, &path, &dir, mode, started) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let skip_cleanup = matches!(
                err.downcast_ref::<CloneError>(),
                Some(CloneError::DestNotEmpty(_))
            );
            if !skip_cleanup {
                cleanup_failed_clone(&path, dest_preexisted_empty);
            }
            Err(err)
        }
    }
}

fn clone_into(
    url: &str,
    path: &Path,
    dir: &str,
    mode: OutputMode,
    started: Instant,
) -> Result<CloneOutcome> {
    let factory = DefaultHttpClientFactory;
    let clone_opts = CloneOptions {
        url: url.to_owned(),
        dest: path.to_path_buf(),
        environment: crate::context::environment(),
        remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
        initial_branch: "main".to_owned(),
        thread_policy: crate::context::thread_policy_for_repo(),
    };
    let mut term_progress = TerminalProgress::new(mode);
    let outcome = clone_repo(clone_opts, &mut term_progress, Some(&factory))?;

    let tree = context::commit_tree(&outcome.repo, &outcome.checkout_oid)?;
    let progress_enabled =
        mode == OutputMode::Human && crate::terminal_progress::stderr_progress_enabled();
    let mut progress = crate::terminal_progress::TerminalProgress::new(progress_enabled);
    checkout_between_trees_with_progress(&outcome.repo, None, &tree, &mut progress)
        .context("could not check out files")?;
    let index = outcome.repo.load_index().context("could not read index")?;
    let files = u64::try_from(index.entries().len()).unwrap_or(u64::MAX);

    Ok(CloneOutcome {
        url: url.to_owned(),
        path: dir.to_owned(),
        branch: outcome.branch,
        objects: outcome.transfer.objects,
        bytes: outcome.transfer.bytes,
        branches: outcome.branches,
        tags: outcome.tags,
        files,
        elapsed: started.elapsed(),
    })
}

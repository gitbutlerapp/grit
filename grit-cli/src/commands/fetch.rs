//! `grit fetch` — download refs and objects from a remote (default `origin`).

use crate::output::OutputMode;
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::{DefaultHttpClientFactory, RemoteError, DEFAULT_REMOTE};
use grit_lib::transfer::{resolve_fetch_prune, FetchOptions, TagMode};
use serde::Serialize;
use std::io::Write;

use crate::commands::auth;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit fetch`: the refs that changed.
#[derive(Serialize)]
pub struct FetchOutcome {
    pub remote: String,
    pub updates: Vec<FetchUpdate>,
    pub updated: usize,
}

/// One updated tracking ref. `old_oid`/`new_oid` are full hex, or `null` for a
/// newly-created (`old_oid`) or deleted (`new_oid`) ref.
#[derive(Serialize)]
pub struct FetchUpdate {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
}

impl HumanRender for FetchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for update in &self.updates {
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            outln!(w, "  {}  {from} → {to}", update.ref_name);
        }
        if self.updated == 0 {
            outln!(w, "Already up to date with {}.", self.remote);
        } else {
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

pub fn run(
    remote: Option<String>,
    prune: bool,
    no_prune: bool,
    mode: OutputMode,
) -> Result<FetchOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    let remote_name = remote.unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
    let cli_prune = if prune {
        Some(true)
    } else if no_prune {
        Some(false)
    } else {
        None
    };
    let prune_enabled = resolve_fetch_prune(&config, &remote_name, cli_prune)
        .context("could not resolve fetch prune settings")?;

    let factory = DefaultHttpClientFactory;
    let result = match fetch_once(&repo, &config, &remote_name, prune_enabled, &factory, mode) {
        Ok(result) => result,
        Err(err) => {
            let url = config
                .get(&format!("remote.{remote_name}.url"))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                fetch_once(&repo, &config, &remote_name, prune_enabled, &factory, mode)?
            } else {
                return Err(err);
            }
        }
    };

    let updates: Vec<FetchUpdate> = result
        .updates
        .iter()
        .filter(|update| update.old_oid != update.new_oid)
        .filter_map(|update| {
            let ref_name = update.local_ref.clone()?;
            Some(FetchUpdate {
                ref_name,
                old_oid: update.old_oid.as_ref().map(|o| o.to_hex()),
                new_oid: update.new_oid.as_ref().map(|o| o.to_hex()),
            })
        })
        .collect();
    let updated = updates.len();

    Ok(FetchOutcome {
        remote: remote_name,
        updates,
        updated,
    })
}

fn fetch_once(
    repo: &grit_lib::repo::Repository,
    config: &ConfigSet,
    remote_name: &str,
    prune: bool,
    factory: &DefaultHttpClientFactory,
    mode: OutputMode,
) -> Result<grit_lib::transfer::FetchOutcome> {
    let remote = crate::commands::remote::resolve_remote_or_url(Some(config), remote_name)?;
    let mut progress = TerminalProgress::new(mode);
    remote
        .fetch(
            repo,
            FetchOptions {
                tags: TagMode::Following,
                thread_policy: repo.thread_policy(),
                environment: crate::context::environment(),
                prune,
                ..Default::default()
            },
            &mut progress,
            Some(factory),
        )
        .map_err(map_remote_fetch_error)
}

fn map_remote_fetch_error(err: RemoteError) -> anyhow::Error {
    if matches!(
        &err,
        RemoteError::Library(grit_lib::error::Error::BundleMissingPrerequisites)
    ) {
        return anyhow::Error::new(crate::commands::bundle::MissingPrerequisites);
    }
    anyhow::Error::msg(err.to_string())
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

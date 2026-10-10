//! `grit fetch` — download refs and objects from a remote (default `origin`).

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::fetch::NoProgress;
use grit_lib::remote::{DefaultHttpClientFactory, Remote, DEFAULT_REMOTE};
use grit_lib::transfer::{FetchOptions, TagMode};
use serde::Serialize;

use crate::commands::auth;
use crate::context;
use crate::output::HumanRender;

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
    fn render_human(&self) {
        for update in &self.updates {
            let from = update.old_oid.as_deref().map_or("new", short_hex);
            let to = update.new_oid.as_deref().map_or("deleted", short_hex);
            println!("  {}  {from} → {to}", update.ref_name);
        }
        if self.updated == 0 {
            println!("Already up to date with {}.", self.remote);
        } else {
            println!(
                "Fetched {} update{} from {}.",
                self.updated,
                plural(self.updated),
                self.remote
            );
        }
    }
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(remote: Option<String>) -> Result<FetchOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    let remote_name = remote.unwrap_or_else(|| DEFAULT_REMOTE.to_owned());

    let factory = DefaultHttpClientFactory;
    let result = match fetch_once(&repo, &config, &remote_name, &factory) {
        Ok(result) => result,
        Err(err) => {
            let url = config
                .get(&format!("remote.{remote_name}.url"))
                .unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                fetch_once(&repo, &config, &remote_name, &factory)?
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
    factory: &DefaultHttpClientFactory,
) -> Result<grit_lib::transfer::FetchOutcome> {
    let remote =
        Remote::from_config(config, remote_name).map_err(|e| anyhow::Error::msg(e.to_string()))?;
    remote
        .fetch(
            repo,
            FetchOptions {
                tags: TagMode::Following,
                ..Default::default()
            },
            &mut NoProgress,
            Some(factory),
        )
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

//! `grit fetch` — download refs and objects from a remote (default `origin`).

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use serde::Serialize;

use crate::commands::auth;
use crate::context;
use crate::net;
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
    let remote = remote.unwrap_or_else(|| net::DEFAULT_REMOTE.to_owned());

    let refspecs = net::fetch_refspecs(&config, &remote);
    // On an HTTPS auth failure, `grit auth` can refresh the token and we retry once.
    let result = match net::fetch(&repo, &config, &remote, refspecs.clone()) {
        Ok(result) => result,
        Err(err) => {
            let url = net::remote_url(&config, &remote).unwrap_or_default();
            if auth::offer_reauth(&err, &url)? {
                net::fetch(&repo, &config, &remote, refspecs)?
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
                old_oid: update
                    .old_oid
                    .as_ref()
                    .map(grit_lib::objects::ObjectId::to_hex),
                new_oid: update
                    .new_oid
                    .as_ref()
                    .map(grit_lib::objects::ObjectId::to_hex),
            })
        })
        .collect();

    Ok(FetchOutcome {
        remote,
        updated: updates.len(),
        updates,
    })
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

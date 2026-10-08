//! `grit remote` — list remotes, or add one.

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;

/// Result of `grit remote`, tagged by `action` (`list` / `add`).
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteOutcome {
    List { remotes: Vec<RemoteEntry> },
    Add { name: String, url: String },
}

/// One remote in a `list` outcome.
#[derive(Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
}

impl HumanRender for RemoteOutcome {
    fn render_human(&self) {
        match self {
            RemoteOutcome::List { remotes } => {
                if remotes.is_empty() {
                    println!("No remotes. Add one with: grit remote add <name> <url>");
                    return;
                }
                for remote in remotes {
                    println!("{}\t{}", remote.name, remote.url);
                }
            }
            RemoteOutcome::Add { name, url } => println!("Added remote {name} → {url}"),
        }
    }
}

/// `Some((name, url))` adds a remote; `None` lists them.
pub fn run(add: Option<(String, String)>) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    match add {
        None => list(&repo),
        Some((name, url)) => add_remote(&repo, &name, &url),
    }
}

fn list(repo: &Repository) -> Result<RemoteOutcome> {
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    let remotes = remote_names(&config)
        .into_iter()
        .map(|name| {
            let url = config
                .get(&format!("remote.{name}.url"))
                .unwrap_or_else(|| "(no url)".to_owned());
            RemoteEntry { name, url }
        })
        .collect();
    Ok(RemoteOutcome::List { remotes })
}

fn add_remote(repo: &Repository, name: &str, url: &str) -> Result<RemoteOutcome> {
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .context("could not load config")?;
    if remote_names(&config).iter().any(|n| n == name) {
        bail!("remote '{name}' already exists");
    }

    let path = repo.git_dir.join("config");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Local)
        .context("could not parse repository config")?;
    cfg.set(&format!("remote.{name}.url"), url)?;
    cfg.set(
        &format!("remote.{name}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{name}/*"),
    )?;
    cfg.write().context("could not write repository config")?;

    Ok(RemoteOutcome::Add {
        name: name.to_owned(),
        url: url.to_owned(),
    })
}

/// Distinct, sorted remote names from any `remote.<name>.*` config entry.
fn remote_names(config: &ConfigSet) -> Vec<String> {
    let mut names: Vec<String> = config
        .entries()
        .iter()
        .filter_map(|entry| {
            let rest = entry.key.strip_prefix("remote.")?;
            // `remote.<name>.<key>` — the name is everything before the last dot.
            rest.rsplit_once('.').map(|(name, _)| name.to_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

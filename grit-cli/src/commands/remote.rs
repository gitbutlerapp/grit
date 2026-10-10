//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::remote::{DefaultHttpClientFactory, ListRefsOptions, Remote, RemoteRef};
use grit_lib::repo::Repository;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::context;
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit remote`, tagged by `action` (`list` / `add` / `refs`).
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteOutcome {
    List { remotes: Vec<RemoteEntry> },
    Add { name: String, url: String },
    Refs {
        refs: Vec<RemoteRefEntry>,
        #[serde(skip)]
        lines: Vec<RemoteRefLine>,
    },
}

/// One remote in a `list` outcome.
#[derive(Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
}

/// One ref in a `refs` JSON outcome (annotated tags carry `peeled`; no separate `^{}` row).
#[derive(Serialize, Clone)]
pub struct RemoteRefEntry {
    pub name: String,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peeled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symref_target: Option<String>,
}

/// One line of human/`git ls-remote`-style output (`oid` then `name`).
#[derive(Clone)]
pub struct RemoteRefLine {
    pub oid: String,
    pub name: String,
    pub symref: bool,
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
            RemoteOutcome::Refs { lines, .. } => {
                for line in lines {
                    if line.symref {
                        println!("ref: {}\t{}", line.oid, line.name);
                    } else {
                        println!("{}\t{}", line.oid, line.name);
                    }
                }
            }
        }
    }
}

impl MarkdownRender for RemoteOutcome {
    fn render_markdown(&self) {
        let RemoteOutcome::Refs { refs, .. } = self else {
            return;
        };
        println!("## Remote refs\n");
        if refs.is_empty() {
            println!("No refs matched.\n");
            return;
        }
        println!("| Ref | Object | Peeled | Symref target |");
        println!("| --- | --- | --- | --- |");
        for entry in refs {
            let peeled = entry.peeled.as_deref().unwrap_or("—");
            let sym = entry.symref_target.as_deref().unwrap_or("—");
            println!(
                "| `{}` | `{}` | {} | {} |",
                entry.name,
                entry.oid,
                if peeled == "—" {
                    "—".to_owned()
                } else {
                    format!("`{peeled}`")
                },
                if sym == "—" {
                    "—".to_owned()
                } else {
                    format!("`{sym}`")
                }
            );
        }
        println!();
    }
}

/// `Some((name, url))` adds a remote; `None` lists them.
pub fn run_list_or_add(add: Option<(String, String)>) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    match add {
        None => list(&repo),
        Some((name, url)) => add_remote(&repo, &name, &url),
    }
}

pub fn run_refs(
    remote_or_url: &str,
    heads: bool,
    tags: bool,
    prefixes: Vec<String>,
) -> Result<RemoteOutcome> {
    let repo = context::discover().ok();
    let config = repo
        .as_ref()
        .map(|r| {
            ConfigSet::load(&crate::context::environment(), Some(&r.git_dir), true)
                .context("could not load config")
        })
        .transpose()?;
    let remote = resolve_remote_or_url(config.as_ref(), remote_or_url)?;
    let opts = ListRefsOptions {
        prefixes,
        heads,
        tags,
        peel: true,
        ..Default::default()
    };
    let factory = DefaultHttpClientFactory;
    let raw = remote
        .list_refs(repo.as_ref(), &opts, Some(&factory))
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;
    let (refs, lines) = map_remote_refs(&raw);
    Ok(RemoteOutcome::Refs { refs, lines })
}

fn resolve_remote_or_url(config: Option<&ConfigSet>, remote_or_url: &str) -> Result<Remote> {
    if let Some(config) = config {
        if remote_names(config).iter().any(|n| n == remote_or_url) {
            return Remote::from_config(config, remote_or_url)
                .map_err(|e| anyhow::Error::msg(e.to_string()));
        }
    }
    if looks_like_url_or_path(remote_or_url) {
        return Remote::from_url(remote_or_url).map_err(|e| anyhow::Error::msg(e.to_string()));
    }
    if local_path_argument(remote_or_url).is_some() {
        return Remote::from_url(remote_or_url).map_err(|e| anyhow::Error::msg(e.to_string()));
    }
    bail!(
        "unknown remote '{remote_or_url}' (not configured — pass a URL or path, or use `grit remote add`)"
    );
}

/// True when `s` is an existing filesystem path (relative or absolute).
fn local_path_argument(s: &str) -> Option<PathBuf> {
    let path = Path::new(s);
    if path.exists() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

fn looks_like_url_or_path(s: &str) -> bool {
    s.contains("://")
        || s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.contains(':')
        || s.ends_with(".git")
}

fn map_remote_refs(raw: &[RemoteRef]) -> (Vec<RemoteRefEntry>, Vec<RemoteRefLine>) {
    let mut peel_by_tag: HashMap<String, String> = HashMap::new();
    for entry in raw {
        if let Some(base) = entry.name.strip_suffix("^{}") {
            peel_by_tag.insert(base.to_owned(), entry.oid.to_hex());
        }
    }

    let mut lines: Vec<RemoteRefLine> = raw
        .iter()
        .flat_map(|entry| {
            let mut out = Vec::new();
            if let Some(target) = &entry.symref_target {
                if entry.name == "HEAD" {
                    out.push(RemoteRefLine {
                        oid: target.clone(),
                        name: entry.name.clone(),
                        symref: true,
                    });
                }
            }
            out.push(RemoteRefLine {
                oid: entry.oid.to_hex(),
                name: entry.name.clone(),
                symref: false,
            });
            out
        })
        .collect();
    lines.sort_by(|a, b| a.name.cmp(&b.name).then(a.symref.cmp(&b.symref)));

    let refs: Vec<RemoteRefEntry> = raw
        .iter()
        .filter(|e| !e.name.ends_with("^{}"))
        .map(|entry| RemoteRefEntry {
            name: entry.name.clone(),
            oid: entry.oid.to_hex(),
            peeled: peel_by_tag.get(&entry.name).cloned(),
            symref_target: entry.symref_target.clone(),
        })
        .collect();

    (refs, lines)
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
            rest.rsplit_once('.').map(|(name, _)| name.to_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

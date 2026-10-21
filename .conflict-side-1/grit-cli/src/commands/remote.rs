//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::remote::{DefaultHttpClientFactory, ListRefsOptions, Remote, RemoteRef};
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

/// Result of `grit remote`, tagged by `action` (`list` / `add` / `refs`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteOutcome {
    List {
        remotes: Vec<RemoteEntry>,
    },
    Add {
        name: String,
        url: String,
    },
    Refs {
        refs: Vec<RemoteRefEntry>,
        #[serde(skip)]
        #[schemars(skip)]
        lines: Vec<RemoteRefLine>,
    },
}

/// One remote in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
}

/// One ref in a `refs` JSON outcome (annotated tags carry `peeled`; no separate `^{}` row).
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteRefEntry {
    pub name: String,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub peeled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
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
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            RemoteOutcome::List { remotes } => {
                if remotes.is_empty() {
                    outln!(w, "No remotes. Add one with: grit remote add <name> <url>");
                    return Ok(());
                }
                for remote in remotes {
                    outln!(w, "{}\t{}", remote.name, remote.url);
                }
            }
            RemoteOutcome::Add { name, url } => outln!(w, "Added remote {name} → {url}"),
            RemoteOutcome::Refs { lines, .. } => {
                for line in lines {
                    if line.symref {
                        outln!(w, "ref: {}\t{}", line.oid, line.name);
                    } else {
                        outln!(w, "{}\t{}", line.oid, line.name);
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RemoteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            RemoteOutcome::List { remotes } => markdown::print_remote_list(w, remotes)?,
            RemoteOutcome::Add { name, url } => outln!(w, "Added remote `{name}` → {url}."),
            RemoteOutcome::Refs { refs, .. } => {
                outln!(w, "## Remote refs\n");
                if refs.is_empty() {
                    outln!(w, "No refs matched.\n");
                    return Ok(());
                }
                outln!(w, "| Ref | Object | Peeled | Symref target |");
                outln!(w, "| --- | --- | --- | --- |");
                for entry in refs {
                    let peeled = entry.peeled.as_deref().unwrap_or("—");
                    let sym = entry.symref_target.as_deref().unwrap_or("—");
                    let peeled_cell = if peeled == "—" {
                        "—".to_owned()
                    } else {
                        format!("`{peeled}`")
                    };
                    let sym_cell = if sym == "—" {
                        "—".to_owned()
                    } else {
                        format!("`{sym}`")
                    };
                    outln!(
                        w,
                        "| `{}` | `{}` | {} | {} |",
                        entry.name,
                        entry.oid,
                        peeled_cell,
                        sym_cell
                    );
                }
                outln!(w);
            }
        }
        Ok(())
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
    mode: OutputMode,
) -> Result<RemoteOutcome> {
    let opts = ListRefsOptions {
        prefixes,
        heads,
        tags,
        peel: true,
        // Match `git ls-remote` on smart HTTP: use the ref advertisement from the
        // initial `info/refs` GET instead of a v2 `ls-refs` POST when both work.
        protocol_version: Some(0),
        ..Default::default()
    };
    let factory = DefaultHttpClientFactory;
    let repo = context::discover().ok();
    let env = crate::context::environment();
    let config = match repo.as_ref() {
        Some(r) => {
            ConfigSet::load(&env, Some(r.git_dir()), true).context("could not load config")?
        }
        None => ConfigSet::load(&env, None, true).context("could not load config")?,
    };
    let remote = resolve_remote_or_url(Some(&config), remote_or_url)?;
    let raw = remote
        .list_refs(repo.as_ref(), &opts, Some(&factory))
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;
    let (refs, lines) = match mode {
        OutputMode::Human => (Vec::new(), remote_ref_lines(&raw)),
        _ => map_remote_refs(&raw),
    };
    Ok(RemoteOutcome::Refs { refs, lines })
}

pub(crate) fn resolve_remote_or_url(
    config: Option<&ConfigSet>,
    remote_or_url: &str,
) -> Result<Remote> {
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
        || s.ends_with(".bundle")
}

fn remote_ref_lines(raw: &[RemoteRef]) -> Vec<RemoteRefLine> {
    let mut lines: Vec<RemoteRefLine> = Vec::with_capacity(raw.len() + 1);
    lines.extend(raw.iter().flat_map(|entry| {
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
    }));
    lines
}

fn map_remote_refs(raw: &[RemoteRef]) -> (Vec<RemoteRefEntry>, Vec<RemoteRefLine>) {
    let mut peel_by_tag: HashMap<String, String> = HashMap::new();
    peel_by_tag.reserve(raw.len() / 8);
    for entry in raw {
        if let Some(base) = entry.name.strip_suffix("^{}") {
            peel_by_tag.insert(base.to_owned(), entry.oid.to_hex());
        }
    }

    let lines = remote_ref_lines(raw);

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
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
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
    let config = ConfigSet::load(&crate::context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    if remote_names(&config).iter().any(|n| n == name) {
        bail!("remote '{name}' already exists");
    }

    let path = repo.git_dir().join("config");
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

//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::refs::listing::{RefSort, SortKey};
use grit_lib::remote::fetch_request::resolve_remote_argument;
use grit_lib::remote::{fold_peeled_refs, DefaultHttpClientFactory, ListRefsOptions, RemoteRef};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{RemoteUrlKind, UrlChange};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::fetch::map_remote_error;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};

/// Result of `grit remote`, tagged by `action` (`list` / `add` / `refs`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteOutcome {
    List {
        remotes: Vec<RemoteEntry>,
        #[serde(skip)]
        #[schemars(skip)]
        verbose_lines: Vec<RemoteVerboseLine>,
    },
    Add {
        name: String,
        url: String,
    },
    SetUrl {
        name: String,
        kind: String,
        old_url: Option<String>,
        url: String,
        dry_run: bool,
    },
    Rename {
        old: String,
        new: String,
        refs_renamed: usize,
        refspecs_updated: Vec<String>,
        branches_updated: Vec<String>,
        dry_run: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub fetch_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub push_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub refspecs: Option<Vec<String>>,
}

/// One line of `grit remote -v` output.
#[derive(Clone)]
pub struct RemoteVerboseLine {
    pub name: String,
    pub url: String,
    pub kind: RemoteVerboseKind,
}

#[derive(Clone, Copy)]
pub enum RemoteVerboseKind {
    Fetch,
    Push,
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
            RemoteOutcome::List {
                remotes,
                verbose_lines,
            } => {
                if remotes.is_empty() {
                    outln!(w, "No remotes. Add one with: grit remote add <name> <url>");
                    return Ok(());
                }
                if !verbose_lines.is_empty() {
                    for line in verbose_lines {
                        let label = match line.kind {
                            RemoteVerboseKind::Fetch => "fetch",
                            RemoteVerboseKind::Push => "push",
                        };
                        outln!(w, "{}\t{} ({label})", line.name, line.url);
                    }
                } else {
                    for remote in remotes {
                        outln!(w, "{}\t{}", remote.name, remote.url);
                    }
                }
            }
            RemoteOutcome::Add { name, url } => outln!(w, "Added remote {name} → {url}"),
            RemoteOutcome::SetUrl {
                name,
                kind,
                old_url,
                url,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                if let Some(old) = old_url {
                    outln!(w, "{prefix}Updated remote {name} {kind} URL: {old} → {url}");
                } else {
                    outln!(w, "{prefix}Set remote {name} {kind} URL to {url}");
                }
            }
            RemoteOutcome::Rename {
                old,
                new,
                refs_renamed,
                dry_run,
                ..
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                outln!(
                    w,
                    "{prefix}Renamed remote {old} → {new} ({refs_renamed} tracking refs updated)"
                );
            }
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
            RemoteOutcome::List {
                remotes,
                verbose_lines: _,
            } => markdown::print_remote_list(w, remotes)?,
            RemoteOutcome::Add { name, url } => outln!(w, "Added remote `{name}` → {url}."),
            RemoteOutcome::SetUrl {
                name,
                kind,
                old_url,
                url,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                if let Some(old) = old_url {
                    outln!(
                        w,
                        "Updated remote `{name}` {kind} URL{dry}: `{old}` → {url}."
                    );
                } else {
                    outln!(w, "Set remote `{name}` {kind} URL{dry} to {url}.");
                }
            }
            RemoteOutcome::Rename {
                old,
                new,
                refs_renamed,
                refspecs_updated,
                branches_updated,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(
                    w,
                    "Renamed remote `{old}` → `{new}`{dry} ({refs_renamed} tracking refs)."
                );
                if !refspecs_updated.is_empty() {
                    outln!(w, "\nUpdated fetch refspecs:");
                    for spec in refspecs_updated {
                        outln!(w, "- `{spec}`");
                    }
                }
                if !branches_updated.is_empty() {
                    outln!(
                        w,
                        "\nUpdated branch upstream: `{}`.",
                        branches_updated.join("`, `")
                    );
                }
            }
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
pub fn run_list_or_add(add: Option<(String, String)>, verbose: bool) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    match add {
        None => list(&repo, verbose),
        Some((name, url)) => add_remote(&repo, &name, &url),
    }
}

pub fn run_set_url(name: &str, url: &str, push: bool, dry_run: bool) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    let kind = if push {
        RemoteUrlKind::Push
    } else {
        RemoteUrlKind::Fetch
    };
    let change = repo
        .remotes()
        .set_url(name, url, kind, dry_run)
        .map_err(map_remote_error)?;
    Ok(set_url_outcome(name, kind, change, dry_run))
}

fn set_url_outcome(
    name: &str,
    kind: RemoteUrlKind,
    change: UrlChange,
    dry_run: bool,
) -> RemoteOutcome {
    let kind_label = match kind {
        RemoteUrlKind::Fetch => "fetch",
        RemoteUrlKind::Push => "push",
    };
    RemoteOutcome::SetUrl {
        name: name.to_owned(),
        kind: kind_label.to_owned(),
        old_url: change.old,
        url: change.new,
        dry_run,
    }
}

/// Flags for `grit remote refs` (sort, limit, namespaces, patterns).
#[derive(Debug, Clone)]
pub struct RemoteRefsCliOptions {
    /// Only `refs/heads/`.
    pub heads: bool,
    /// Only `refs/tags/`.
    pub tags: bool,
    /// Sort key from argv.
    pub sort: crate::RemoteRefSortArg,
    /// Reverse sort order.
    pub reverse: bool,
    /// Maximum lines after sorting.
    pub limit: Option<usize>,
    /// `git ls-remote`-style ref patterns.
    pub prefixes: Vec<String>,
}

pub fn run_refs(
    remote_or_url: &str,
    cli: &RemoteRefsCliOptions,
    mode: OutputMode,
) -> Result<RemoteOutcome> {
    let factory = DefaultHttpClientFactory;
    let repo = context::discover().ok();
    let env = crate::context::environment();
    let config = match repo.as_ref() {
        Some(r) => {
            ConfigSet::load(&env, Some(r.git_dir()), true).context("could not load config")?
        }
        None => ConfigSet::load(&env, None, true).context("could not load config")?,
    };
    let sort_key = match cli.sort {
        crate::RemoteRefSortArg::Name => SortKey::Name,
        crate::RemoteRefSortArg::Version => SortKey::Version,
    };
    let opts = ListRefsOptions {
        prefixes: cli.prefixes.clone(),
        heads: cli.heads,
        tags: cli.tags,
        peel: true,
        sort: RefSort {
            key: sort_key,
            reverse: cli.reverse,
        },
        limit: cli.limit,
        versionsort_suffixes: config.get_all("versionsort.suffix"),
        // Match `git ls-remote` on smart HTTP: use the ref advertisement from the
        // initial `info/refs` GET instead of a v2 `ls-refs` POST when both work.
        protocol_version: Some(0),
        ..Default::default()
    };
    let remote = resolve_remote_argument(&config, remote_or_url).map_err(map_remote_error)?;
    let raw = remote
        .list_refs(repo.as_ref(), &opts, Some(&factory))
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;
    let (refs, lines) = match mode {
        OutputMode::Human => (Vec::new(), remote_ref_lines(&raw)),
        _ => map_remote_refs(&raw),
    };
    Ok(RemoteOutcome::Refs { refs, lines })
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
    let refs = fold_peeled_refs(raw)
        .into_iter()
        .map(|entry| RemoteRefEntry {
            name: entry.name,
            oid: entry.oid.to_hex(),
            peeled: entry.peeled.map(|oid| oid.to_hex()),
            symref_target: entry.symref_target,
        })
        .collect();
    (refs, remote_ref_lines(raw))
}

fn list(repo: &Repository, verbose: bool) -> Result<RemoteOutcome> {
    let configured = repo
        .remotes()
        .configured()
        .context("could not load config")?;
    let mut verbose_lines = Vec::new();
    let remotes = configured
        .into_iter()
        .map(|remote| {
            let display_url = remote.url.clone().unwrap_or_else(|| "(no url)".to_owned());
            if verbose {
                if let Some(fetch) = remote.effective_fetch_url.as_deref() {
                    verbose_lines.push(RemoteVerboseLine {
                        name: remote.name.clone(),
                        url: fetch.to_owned(),
                        kind: RemoteVerboseKind::Fetch,
                    });
                }
                if remote.push_urls.is_empty() {
                    if let Some(push) = remote.effective_push_url.as_deref() {
                        verbose_lines.push(RemoteVerboseLine {
                            name: remote.name.clone(),
                            url: push.to_owned(),
                            kind: RemoteVerboseKind::Push,
                        });
                    }
                } else {
                    for push in &remote.effective_push_urls {
                        verbose_lines.push(RemoteVerboseLine {
                            name: remote.name.clone(),
                            url: push.clone(),
                            kind: RemoteVerboseKind::Push,
                        });
                    }
                }
            }
            RemoteEntry {
                name: remote.name.clone(),
                url: display_url,
                fetch_url: verbose.then(|| {
                    remote
                        .effective_fetch_url
                        .clone()
                        .unwrap_or_else(|| "(no url)".to_owned())
                }),
                push_url: verbose.then(|| {
                    remote
                        .effective_push_url
                        .clone()
                        .unwrap_or_else(|| "(no url)".to_owned())
                }),
                refspecs: verbose.then(|| remote.fetch_refspecs.clone()),
            }
        })
        .collect();
    Ok(RemoteOutcome::List {
        remotes,
        verbose_lines,
    })
}

fn add_remote(repo: &Repository, name: &str, url: &str) -> Result<RemoteOutcome> {
    repo.remotes().add(name, url).map_err(map_remote_error)?;
    Ok(RemoteOutcome::Add {
        name: name.to_owned(),
        url: url.to_owned(),
    })
}

/// Rename a configured remote (`grit remote rename <old> <new>`).
pub fn run_rename(old: &str, new: &str, dry_run: bool) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    let out = repo
        .remotes()
        .rename(old, new, dry_run)
        .map_err(map_remote_error)?;
    Ok(RemoteOutcome::Rename {
        old: out.old,
        new: out.new,
        refs_renamed: out.refs_renamed,
        refspecs_updated: out.refspecs_updated,
        branches_updated: out.branches_updated,
        dry_run: out.dry_run,
    })
}

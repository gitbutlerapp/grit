//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::refs::listing::{RefSort, SortKey};
use grit_lib::remote::fetch_request::resolve_remote_argument;
use grit_lib::remote::{
    fold_peeled_refs, DefaultHttpClientFactory, InspectOptions, ListRefsOptions,
    RemoteBranchStateKind, RemoteRef, RemoteSummary,
};
use grit_lib::repo::Repository;
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
    Show {
        #[serde(flatten)]
        summary: RemoteShowJson,
    },
}

/// JSON view of [`RemoteSummary`] for `grit remote show --json`.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowJson {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    pub branches: Vec<RemoteShowBranchJson>,
    pub pull: Vec<RemoteShowPullJson>,
}

/// One branch row in a `show` JSON outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowBranchJson {
    pub name: String,
    pub state: RemoteShowBranchStateJson,
}

/// Branch tracking state in JSON (`tracked` / `new` / `stale`).
#[derive(Serialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RemoteShowBranchStateJson {
    Tracked,
    New,
    Stale,
}

/// One pull-config row in a `show` JSON outcome.
#[derive(Serialize, JsonSchema, Clone)]
pub struct RemoteShowPullJson {
    pub local: String,
    pub merges: String,
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
            RemoteOutcome::Show { summary } => render_show_human(w, summary)?,
        }
        Ok(())
    }
}

impl MarkdownRender for RemoteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            RemoteOutcome::List { remotes } => markdown::print_remote_list(w, remotes)?,
            RemoteOutcome::Add { name, url } => outln!(w, "Added remote `{name}` → {url}."),
            RemoteOutcome::Show { summary } => render_show_markdown(w, summary)?,
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

fn list(repo: &Repository) -> Result<RemoteOutcome> {
    let remotes = repo
        .remotes()
        .configured()
        .context("could not load config")?
        .into_iter()
        .map(|remote| RemoteEntry {
            name: remote.name,
            url: remote.url.unwrap_or_else(|| "(no url)".to_owned()),
        })
        .collect();
    Ok(RemoteOutcome::List { remotes })
}

fn add_remote(repo: &Repository, name: &str, url: &str) -> Result<RemoteOutcome> {
    repo.remotes().add(name, url).map_err(map_remote_error)?;
    Ok(RemoteOutcome::Add {
        name: name.to_owned(),
        url: url.to_owned(),
    })
}

/// CLI flags for `grit remote show`.
#[derive(Debug, Clone, Copy)]
pub struct RemoteShowCliOptions {
    /// When `false`, use local remote-tracking refs only (no network).
    pub query: bool,
    pub limit: Option<usize>,
}

pub fn run_show(name: &str, cli: &RemoteShowCliOptions) -> Result<RemoteOutcome> {
    let factory = DefaultHttpClientFactory;
    let repo = context::discover()?;
    let options = InspectOptions {
        query: cli.query,
        limit: cli.limit,
    };
    let summary = repo
        .remotes()
        .inspect(name, &options, Some(&factory))
        .map_err(map_remote_error)?;
    Ok(RemoteOutcome::Show {
        summary: map_remote_show(&summary),
    })
}

fn map_remote_show(summary: &RemoteSummary) -> RemoteShowJson {
    RemoteShowJson {
        name: summary.name.clone(),
        fetch_url: summary.fetch_url.clone(),
        push_url: summary.push_url.clone(),
        head_branch: summary.head_branch.clone(),
        branches: summary
            .branches
            .iter()
            .map(|b| RemoteShowBranchJson {
                name: b.name.clone(),
                state: match b.state {
                    RemoteBranchStateKind::Tracked => RemoteShowBranchStateJson::Tracked,
                    RemoteBranchStateKind::New => RemoteShowBranchStateJson::New,
                    RemoteBranchStateKind::Stale => RemoteShowBranchStateJson::Stale,
                },
            })
            .collect(),
        pull: summary
            .pull
            .iter()
            .map(|p| RemoteShowPullJson {
                local: p.local.clone(),
                merges: p.merges.clone(),
            })
            .collect(),
    }
}

fn render_show_human(w: &mut dyn Write, summary: &RemoteShowJson) -> Result<()> {
    outln!(w, "* remote {}", summary.name);
    outln!(w, "  Fetch URL: {}", summary.fetch_url);
    outln!(w, "  Push  URL: {}", summary.push_url);
    if let Some(head) = &summary.head_branch {
        outln!(w, "  HEAD branch: {head}");
    }
    outln!(w, "  Remote branches:");
    if summary.branches.is_empty() {
        outln!(w, "    (none)");
    } else {
        for branch in &summary.branches {
            let state = match branch.state {
                RemoteShowBranchStateJson::Tracked => "tracked",
                RemoteShowBranchStateJson::New => "new",
                RemoteShowBranchStateJson::Stale => "stale",
            };
            outln!(w, "    {} {}", branch.name, state);
        }
    }
    if !summary.pull.is_empty() {
        outln!(w, "  Local branches configured for pull:");
        for row in &summary.pull {
            let merge_short = row
                .merges
                .strip_prefix("refs/heads/")
                .unwrap_or(row.merges.as_str());
            outln!(w, "    {} merges with remote {}", row.local, merge_short);
        }
    }
    Ok(())
}

fn render_show_markdown(w: &mut dyn Write, summary: &RemoteShowJson) -> Result<()> {
    outln!(w, "## Remote `{}`\n", summary.name);
    outln!(w, "| | |");
    outln!(w, "| --- | --- |");
    outln!(w, "| Fetch URL | {} |", summary.fetch_url);
    outln!(w, "| Push URL | {} |", summary.push_url);
    if let Some(head) = &summary.head_branch {
        outln!(w, "| HEAD branch | `{head}` |");
    }
    outln!(w);
    outln!(w, "### Remote branches\n");
    if summary.branches.is_empty() {
        outln!(w, "No remote branches.\n");
    } else {
        outln!(w, "| Branch | State |");
        outln!(w, "| --- | --- |");
        for branch in &summary.branches {
            let state = match branch.state {
                RemoteShowBranchStateJson::Tracked => "tracked",
                RemoteShowBranchStateJson::New => "new",
                RemoteShowBranchStateJson::Stale => "stale",
            };
            outln!(w, "| `{}` | {} |", branch.name, state);
        }
        outln!(w);
    }
    if !summary.pull.is_empty() {
        outln!(w, "### Pull configuration\n");
        outln!(w, "| Local branch | Merges |");
        outln!(w, "| --- | --- |");
        for row in &summary.pull {
            outln!(w, "| `{}` | `{}` |", row.local, row.merges);
        }
        outln!(w);
    }
    Ok(())
}

//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::refs::listing::{RefSort, SortKey};
use grit_lib::remote::fetch_request::resolve_remote_argument;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::remote::{
    fold_peeled_refs, DefaultHttpClientFactory, InspectOptions, ListRefsOptions,
    RemoteBranchStateKind, RemoteError, RemoteRef, RemoteSummary,
};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{RemoteUrlKind, RemoveOptions, SetHead, SetHeadOutcome, UrlChange};
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
        fetched: bool,
    },
    SetUrl {
        name: String,
        kind: String,
        old_url: Option<String>,
        url: String,
        dry_run: bool,
    },
    Remove {
        name: String,
        refs_deleted: Vec<String>,
        branches_untracked: Vec<String>,
        dry_run: bool,
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
    Prune {
        name: String,
        refs_pruned: Vec<String>,
        dry_run: bool,
    },
    SetHead {
        name: String,
        target: Option<String>,
        previous_target: Option<String>,
        dry_run: bool,
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
            RemoteOutcome::Add { name, url, fetched } => {
                outln!(w, "Added remote {name} → {url}");
                if *fetched {
                    outln!(w, "Fetched from {name}");
                }
            }
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
            RemoteOutcome::Remove {
                name,
                refs_deleted,
                branches_untracked,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would remove remote {name}");
                } else {
                    outln!(w, "Removed remote {name}");
                }
                if !refs_deleted.is_empty() {
                    outln!(w, "  refs deleted: {}", refs_deleted.len());
                }
                if !branches_untracked.is_empty() {
                    outln!(w, "  branches untracked: {}", branches_untracked.join(", "));
                }
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
            RemoteOutcome::Show { summary } => render_show_human(w, summary)?,
            RemoteOutcome::Prune {
                name,
                refs_pruned,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                if refs_pruned.is_empty() {
                    outln!(w, "{prefix}Nothing to prune for {name}");
                } else {
                    for r in refs_pruned {
                        outln!(w, "{prefix}Pruned {r}");
                    }
                }
            }
            RemoteOutcome::SetHead {
                name,
                target,
                previous_target: _,
                dry_run,
            } => {
                let prefix = if *dry_run { "[dry-run] " } else { "" };
                match target {
                    Some(t) => outln!(w, "{prefix}Set remote {name} HEAD → {t}"),
                    None => outln!(w, "{prefix}Deleted remote {name} HEAD symref"),
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
            RemoteOutcome::Add { name, url, fetched } => {
                outln!(w, "Added remote `{name}` → {url}.");
                if *fetched {
                    outln!(w, "\nFetched from `{name}`.\n");
                }
            }
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
            RemoteOutcome::Remove {
                name,
                refs_deleted,
                branches_untracked,
                dry_run,
            } => {
                if *dry_run {
                    outln!(w, "Would remove remote `{name}`.");
                } else {
                    outln!(w, "Removed remote `{name}`.");
                }
                if !refs_deleted.is_empty() {
                    outln!(w, "\n**Refs deleted:** {}\n", refs_deleted.len());
                    for r in refs_deleted {
                        outln!(w, "- `{r}`");
                    }
                }
                if !branches_untracked.is_empty() {
                    outln!(
                        w,
                        "\n**Branches untracked:** {}.\n",
                        branches_untracked.join(", ")
                    );
                }
            }
            RemoteOutcome::Show { summary } => render_show_markdown(w, summary)?,
            RemoteOutcome::Prune {
                name,
                refs_pruned,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(w, "## Prune `{name}`{dry}\n");
                if refs_pruned.is_empty() {
                    outln!(w, "Nothing to prune.\n");
                } else {
                    for r in refs_pruned {
                        outln!(w, "- `{r}`");
                    }
                    outln!(w);
                }
            }
            RemoteOutcome::SetHead {
                name,
                target,
                previous_target,
                dry_run,
            } => {
                let dry = if *dry_run { " (dry-run)" } else { "" };
                outln!(w, "## Set HEAD for `{name}`{dry}\n");
                if let Some(prev) = previous_target {
                    outln!(w, "Previous: `{prev}`\n");
                }
                match target {
                    Some(t) => outln!(w, "Target: `{t}`.\n"),
                    None => outln!(w, "Deleted symref.\n"),
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

/// `Some((name, url, fetch))` adds a remote; `None` lists them.
pub fn run_list_or_add(
    add: Option<(String, String, bool)>,
    verbose: bool,
) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    match add {
        None => list(&repo, verbose),
        Some((name, url, fetch)) => add_remote(&repo, &name, &url, fetch),
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

fn add_remote(repo: &Repository, name: &str, url: &str, fetch: bool) -> Result<RemoteOutcome> {
    repo.remotes().add(name, url).map_err(map_remote_error)?;
    if fetch {
        let factory = DefaultHttpClientFactory;
        let remote = repo.remotes().get(name).map_err(map_remote_error)?;
        let mut progress = NoProgress;
        repo.remotes()
            .fetch(&remote, FetchOptions::default(), &mut progress, Some(&factory))
            .map_err(map_remote_error)?;
    }
    Ok(RemoteOutcome::Add {
        name: name.to_owned(),
        url: url.to_owned(),
        fetched: fetch,
    })
}

pub fn run_prune(name: &str, dry_run: bool) -> Result<RemoteOutcome> {
    let factory = DefaultHttpClientFactory;
    let repo = context::discover()?;
    let refs = repo
        .remotes()
        .prune(name, dry_run, Some(&factory))
        .map_err(|e| match e {
            RemoteError::UnknownRemote(_) => {
                anyhow::Error::from(RemoteUsageError).context(map_remote_error(e))
            }
            other => map_remote_error(other),
        })?;
    Ok(RemoteOutcome::Prune {
        name: name.to_owned(),
        refs_pruned: refs,
        dry_run,
    })
}

pub fn run_set_head(name: &str, mode: SetHead, dry_run: bool) -> Result<RemoteOutcome> {
    let factory = DefaultHttpClientFactory;
    let repo = context::discover()?;
    let outcome = repo
        .remotes()
        .set_head(name, mode, dry_run, Some(&factory))
        .map_err(|e| match e {
            RemoteError::UnknownRemote(_) => {
                anyhow::Error::from(RemoteUsageError).context(map_remote_error(e))
            }
            other => map_remote_error(other),
        })?;
    Ok(map_set_head_outcome(name, outcome))
}

fn map_set_head_outcome(name: &str, outcome: SetHeadOutcome) -> RemoteOutcome {
    RemoteOutcome::SetHead {
        name: name.to_owned(),
        target: outcome.target,
        previous_target: outcome.previous_target,
        dry_run: outcome.dry_run,
    }
}

/// Marker for usage errors (exit 2) from remote commands.
#[derive(Debug)]
pub struct RemoteUsageError;

impl std::fmt::Display for RemoteUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote usage error")
    }
}

impl std::error::Error for RemoteUsageError {}

pub(crate) fn is_remote_usage_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<RemoteUsageError>().is_some())
}

pub fn run_remove(name: &str, dry_run: bool) -> Result<RemoteOutcome> {
    let repo = context::discover()?;
    let removal = repo
        .remotes()
        .remove(name, RemoveOptions { dry_run })
        .map_err(|e| match e {
            RemoteError::UnknownRemote(_) => {
                anyhow::Error::from(RemoteUsageError).context(map_remote_error(e))
            }
            other => map_remote_error(other),
        })?;
    Ok(RemoteOutcome::Remove {
        name: removal.name,
        refs_deleted: removal.refs_deleted,
        branches_untracked: removal.branches_untracked,
        dry_run: removal.dry_run,
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

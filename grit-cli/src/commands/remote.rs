//! `grit remote` — list remotes, add one, or list refs on a remote.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::progress::NullProgress;
use grit_lib::refs::listing::{RefSort, SortKey};
use grit_lib::remote::fetch_request::resolve_remote_argument;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::remote::{
    fold_peeled_refs, InspectOptions, ListRefsOptions, RemoteBranchStateKind, RemoteError,
    RemoteRef, RemoteSummary,
};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{RemoteUrlKind, RemoveOptions, SetHead, SetHeadOutcome, UrlChange};

use crate::commands::fetch::map_remote_error;
use crate::context;
pub use crate::render::remote::{
    RemoteEntry, RemoteOutcome, RemoteRefEntry, RemoteRefLine, RemoteShowBranchJson,
    RemoteShowBranchStateJson, RemoteShowJson, RemoteShowPullJson, RemoteVerboseKind,
    RemoteVerboseLine,
};
use grit_lib::progress::CancelToken;

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

pub fn run_refs(remote_or_url: &str, cli: &RemoteRefsCliOptions) -> Result<RemoteOutcome> {
    let factory = crate::context::http_client_factory();
    let repo = context::discover().ok();
    let env = crate::context::environment();
    let config = match repo.as_ref() {
        Some(r) => ConfigSet::load(
            &env,
            Some(r.git_dir()),
            &grit_lib::config::LoadConfigOptions::standard(&env, Some(r.git_dir())),
        )
        .context("could not load config")?,
        None => ConfigSet::load(
            &env,
            None,
            &grit_lib::config::LoadConfigOptions::standard(&env, None),
        )
        .context("could not load config")?,
    };
    let sort_key = match cli.sort {
        crate::RemoteRefSortArg::Name => SortKey::Name,
        crate::RemoteRefSortArg::Version => SortKey::Version,
    };
    let protocol_version =
        grit_lib::remote::protocol::try_effective_client_protocol_version_from_config(&config)
            .context("could not read protocol.version")?;
    let opts = ListRefsOptions::default()
        .with_prefixes(cli.prefixes.clone())
        .with_heads(cli.heads)
        .with_tags(cli.tags)
        .with_peel(true)
        .with_sort(
            RefSort::default()
                .with_key(sort_key)
                .with_reverse(cli.reverse),
        )
        .with_limit(cli.limit)
        .with_versionsort_suffixes(crate::context::config_strings(
            &config,
            "versionsort.suffix",
        ))
        .with_protocol_version(Some(protocol_version));
    let remote = resolve_remote_argument(&config, remote_or_url).map_err(map_remote_error)?;
    let raw = remote
        .list_refs(repo.as_ref(), &opts, Some(&factory))
        .map_err(map_remote_error)?;
    let (refs, lines) = map_remote_refs(&raw);
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
        let factory = crate::context::http_client_factory();
        let remote = repo.remotes().get(name).map_err(map_remote_error)?;
        let mut progress = NullProgress;
        repo.remotes()
            .fetch(
                &remote,
                FetchOptions::default(),
                &mut progress,
                &CancelToken::never(),
                Some(&factory),
            )
            .map_err(map_remote_error)?;
    }
    Ok(RemoteOutcome::Add {
        name: name.to_owned(),
        url: url.to_owned(),
        fetched: fetch,
    })
}
pub fn run_prune(name: &str, dry_run: bool) -> Result<RemoteOutcome> {
    let factory = crate::context::http_client_factory();
    let repo = context::discover()?;
    let refs = repo
        .remotes()
        .prune(name, dry_run, Some(&factory))
        .map_err(|e| match e {
            grit_lib::error::Error::Remote(RemoteError::UnknownRemote { .. }) => {
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
    let factory = crate::context::http_client_factory();
    let repo = context::discover()?;
    let outcome = repo
        .remotes()
        .set_head(name, mode, dry_run, Some(&factory))
        .map_err(|e| match e {
            grit_lib::error::Error::Remote(RemoteError::UnknownRemote { .. }) => {
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
        .remove(name, RemoveOptions::default().with_dry_run(dry_run))
        .map_err(|e| match e {
            grit_lib::error::Error::Remote(RemoteError::UnknownRemote { .. }) => {
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
    let factory = crate::context::http_client_factory();
    let repo = context::discover()?;
    let options = InspectOptions::default()
        .with_query(cli.query)
        .with_limit(cli.limit);
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
                    _ => RemoteShowBranchStateJson::Tracked,
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

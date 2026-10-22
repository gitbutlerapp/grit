//! `grit branch` — list branches, or create / delete one.

use anyhow::{Context, Result};
use std::io::Write;

use grit_lib::error::Error;
use grit_lib::porcelain::branch::{create_branch, delete_branch};
use grit_lib::porcelain::branch_list::{
    query_branches, BranchDetail, BranchQuery, BranchUpstreamStatus,
};
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefQueryOptions, RefSort, SortKey};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::resolve_revision;
use schemars::JsonSchema;
use serde::Serialize;

use crate::branch_list_args::BranchListArgs;
use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};

/// Result of `grit branch`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BranchOutcome {
    List {
        /// The current branch, or `null` when detached / unborn.
        current: Option<String>,
        branches: Vec<BranchEntry>,
        #[serde(skip)]
        show_current: bool,
    },
    Create {
        name: String,
    },
    Delete {
        name: String,
        /// Full object id of the branch tip before deletion.
        oid: String,
        /// Abbreviated tip id (seven hex digits when possible).
        short_oid: String,
    },
}

/// Upstream tracking row in JSON.
#[derive(Serialize, JsonSchema)]
pub struct BranchUpstreamEntry {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub gone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
}

/// One branch in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct BranchEntry {
    pub name: String,
    pub current: bool,
    pub full_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<BranchUpstreamEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

impl HumanRender for BranchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List {
                branches,
                show_current,
                current,
                ..
            } => {
                if *show_current {
                    if let Some(name) = current {
                        outln!(w, "{name}");
                    }
                    return Ok(());
                }
                if branches.is_empty() {
                    outln!(w, "No branches yet.");
                    return Ok(());
                }
                for branch in branches {
                    let marker = branch
                        .upstream
                        .as_ref()
                        .map(format_upstream_marker)
                        .unwrap_or_default();
                    let prefix = if branch.current { "* " } else { "  " };
                    if marker.is_empty() {
                        outln!(w, "{prefix}{}", branch.name);
                    } else {
                        outln!(w, "{prefix}{} {marker}", branch.name);
                    }
                }
            }
            BranchOutcome::Create { name } => outln!(w, "Created branch {name}"),
            BranchOutcome::Delete {
                name, short_oid, ..
            } => outln!(w, "Deleted branch {name} (was {short_oid})."),
        }
        Ok(())
    }
}

impl MarkdownRender for BranchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List { branches, .. } => markdown::print_branch_list(w, branches)?,
            BranchOutcome::Create { name } => outln!(w, "Created branch `{name}`."),
            BranchOutcome::Delete {
                name, short_oid, ..
            } => outln!(w, "Deleted branch `{name}` (was `{short_oid}`)."),
        }
        Ok(())
    }
}

/// List, create, or delete a branch depending on `name` and the delete flags.
pub fn run(
    name: Option<String>,
    delete: bool,
    force_delete: bool,
    list: BranchListArgs,
    include_commit_details: bool,
) -> Result<BranchOutcome> {
    let repo = context::discover()?;
    let delete = delete || force_delete;
    match name {
        None => list_branches(&repo, &list, include_commit_details),
        Some(name) if delete => remove(&repo, &name, force_delete),
        Some(name) => create(&repo, &name),
    }
}

fn list_branches(
    repo: &Repository,
    args: &BranchListArgs,
    include_commit_details: bool,
) -> Result<BranchOutcome> {
    let ref_query = build_ref_query(repo, args)?;
    let include_upstream_counts = true;
    let query = BranchQuery {
        ref_query,
        show_current: args.show_current,
        include_commit_details,
        include_upstream_counts,
    };
    let listing = query_branches(repo, &query, &RefQueryOptions::default())
        .context("could not list branches")?;
    Ok(BranchOutcome::List {
        current: listing.current,
        show_current: args.show_current,
        branches: listing
            .branches
            .into_iter()
            .map(branch_entry_from_detail)
            .collect(),
    })
}

fn build_ref_query(repo: &Repository, args: &BranchListArgs) -> Result<RefQuery> {
    let namespaces = if args.all {
        vec![RefNamespace::Heads, RefNamespace::Remotes]
    } else if args.remotes {
        vec![RefNamespace::Remotes]
    } else {
        vec![RefNamespace::Heads]
    };
    let sort_key = match args.sort.as_deref() {
        None | Some("name") => SortKey::Name,
        Some("version") => SortKey::Version,
        Some("date") => SortKey::Date,
        Some(other) => {
            return Err(anyhow::anyhow!(
                "unknown sort key '{other}' (expected name, version, or date)"
            ));
        }
    };
    Ok(RefQuery {
        namespaces,
        patterns: args.patterns.clone(),
        sort: RefSort {
            key: sort_key,
            reverse: args.reverse,
        },
        limit: args.limit,
        contains: resolve_filter_revs(repo, args.contains.as_deref())?,
        merged: resolve_filter_revs(repo, args.merged.as_deref())?,
        no_merged: resolve_filter_revs(repo, args.no_merged.as_deref())?,
        ..RefQuery::default()
    })
}

fn resolve_filter_revs(
    repo: &Repository,
    rev: Option<&str>,
) -> Result<Vec<grit_lib::objects::ObjectId>> {
    let Some(rev) = rev else {
        return Ok(Vec::new());
    };
    let oid = resolve_revision(repo, rev).with_context(|| format!("could not resolve '{rev}'"))?;
    Ok(vec![oid])
}

fn branch_entry_from_detail(detail: BranchDetail) -> BranchEntry {
    let date = detail
        .committer
        .as_deref()
        .map(Person::from_identity_line)
        .map(|p| p.date);
    BranchEntry {
        name: detail.name,
        current: detail.current,
        full_ref: detail.full_ref,
        remote: detail.remote,
        oid: detail.oid.to_hex(),
        subject: detail.subject,
        date,
        upstream: detail.upstream.map(upstream_entry_from_status),
        worktree: detail.worktree.map(|p| p.display().to_string()),
    }
}

fn upstream_entry_from_status(up: BranchUpstreamStatus) -> BranchUpstreamEntry {
    BranchUpstreamEntry {
        ref_name: up.ref_name,
        remote: up.remote,
        gone: up.gone,
        ahead: up.ahead,
        behind: up.behind,
    }
}

fn format_upstream_marker(up: &BranchUpstreamEntry) -> String {
    if up.gone {
        return "[gone]".to_owned();
    }
    match (up.ahead, up.behind) {
        (Some(ahead), Some(behind)) => format!("[{}: ahead {ahead}, behind {behind}]", up.ref_name),
        _ => format!("[{}]", up.ref_name),
    }
}

fn create(repo: &Repository, name: &str) -> Result<BranchOutcome> {
    create_branch(repo, name).context("could not create branch")?;
    Ok(BranchOutcome::Create {
        name: name.to_owned(),
    })
}

fn remove(repo: &Repository, name: &str, force_delete: bool) -> Result<BranchOutcome> {
    let deleted = delete_branch(repo, name, force_delete).map_err(|err| match err {
        Error::BranchNotMerged { .. } => anyhow::anyhow!(
            "{err}.\nIf you are sure you want to delete it, run 'grit branch -D {name}'"
        ),
        other => anyhow::Error::new(other),
    })?;
    let oid = deleted.oid.to_hex();
    let short_oid = oid[..7.min(oid.len())].to_owned();
    Ok(BranchOutcome::Delete {
        name: deleted.name,
        oid,
        short_oid,
    })
}

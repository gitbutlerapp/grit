//! Branch inventory: ref-query listing with upstream, worktree, and commit summary.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::branch_tracking::upstream_tracking_full_ref;
use crate::error::{Error, Result};
use crate::merge_base::count_symmetric_ahead_behind;
use crate::objects::{parse_commit, read_commit_subject_line, ObjectId, ObjectKind};
use crate::refs::listing::{
    query_refs, ref_kind_from_name, RefKind, RefListing, RefNamespace, RefQuery, RefQueryError,
    RefQueryOptions,
};
use crate::repo::Repository;
use crate::rev_parse::abbreviate_ref_name;
use crate::state::{resolve_head_for_repository, HeadState};
use crate::worktree::list_worktrees;

/// Filters and enrichment flags for [`query_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchQuery {
    /// Ref listing filters, sort, and limit (namespaces, patterns, reachability, …).
    pub ref_query: RefQuery,
    /// When set, human callers print only the current branch name (`--show-current`).
    pub show_current: bool,
    /// Load the first-line subject and committer identity line from the ODB.
    pub include_commit_details: bool,
    /// Compute ahead/behind for local branches whose upstream tracking ref resolves.
    pub include_upstream_counts: bool,
}

impl Default for BranchQuery {
    fn default() -> Self {
        Self {
            ref_query: RefQuery {
                namespaces: vec![RefNamespace::Heads],
                ..RefQuery::default()
            },
            show_current: false,
            include_commit_details: false,
            include_upstream_counts: false,
        }
    }
}

/// Upstream tracking status for one local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchUpstreamStatus {
    /// Abbreviated upstream ref (`origin/main`).
    pub ref_name: String,
    /// Configured `branch.<name>.remote`, when set.
    pub remote: Option<String>,
    /// Upstream is configured but the tracking ref does not resolve.
    pub gone: bool,
    /// Commits on the branch not reachable from upstream; `None` when not computed.
    pub ahead: Option<usize>,
    /// Commits on upstream not reachable from the branch; `None` when not computed.
    pub behind: Option<usize>,
}

/// One branch row returned from [`query_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDetail {
    /// Short display name (`main`, `origin/main`, …).
    pub name: String,
    /// Full ref name (`refs/heads/main`, …).
    pub full_ref: String,
    /// Remote name for a remote-tracking ref (`origin` for `refs/remotes/origin/main`).
    pub remote: Option<String>,
    /// Tip object id.
    pub oid: ObjectId,
    /// First line of the tip commit message, when [`BranchQuery::include_commit_details`] is set.
    pub subject: Option<String>,
    /// Raw committer identity line (`Name <email> epoch tz`), when commit details are included.
    pub committer: Option<String>,
    /// Whether `HEAD` is attached to this branch.
    pub current: bool,
    /// Configured upstream for local branches.
    pub upstream: Option<BranchUpstreamStatus>,
    /// Worktree path when this branch is checked out somewhere.
    pub worktree: Option<PathBuf>,
}

/// Result of [`query_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchQueryResult {
    /// Branch `HEAD` is attached to, or `None` when detached / invalid.
    pub current: Option<String>,
    /// Matching branches after filters, sort, and limit.
    pub branches: Vec<BranchDetail>,
}

/// List branches using [`RefQuery`] plus optional upstream, worktree, and commit enrichment.
///
/// # Errors
///
/// Propagates ref-store, object read, and reachability filter errors.
pub fn query_branches(
    repo: &Repository,
    query: &BranchQuery,
    options: &RefQueryOptions,
) -> Result<BranchQueryResult> {
    let listings = query_refs(repo, &query.ref_query, options).map_err(map_ref_query_error)?;
    let current = match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        HeadState::Detached { .. } | HeadState::Invalid => None,
    };
    let worktrees = branch_worktree_paths(repo)?;
    let config = repo.config().ok();
    let config_ref = config.as_deref();
    let mut branches = Vec::with_capacity(listings.len());
    for listing in listings {
        branches.push(detail_from_listing(
            repo,
            listing,
            current.as_deref(),
            &worktrees,
            config_ref,
            query,
        )?);
    }
    Ok(BranchQueryResult { current, branches })
}

fn map_ref_query_error(err: RefQueryError) -> Error {
    match err {
        RefQueryError::Store(e) => e,
        RefQueryError::MissingObjectDatabase => {
            Error::Message("creator-date sort requires an object database".into())
        }
        RefQueryError::MissingObject(oid) => Error::ObjectNotFound(oid.to_hex()),
        RefQueryError::NotCommit(oid) => Error::Message(format!("object {oid} is not a commit")),
    }
}

fn branch_worktree_paths(repo: &Repository) -> Result<HashMap<String, PathBuf>> {
    let mut map = HashMap::new();
    for entry in list_worktrees(repo)? {
        if let HeadState::Branch { short_name, .. } = &entry.head {
            map.insert(short_name.clone(), entry.path);
        }
    }
    Ok(map)
}

fn detail_from_listing(
    repo: &Repository,
    listing: RefListing,
    current: Option<&str>,
    worktrees: &HashMap<String, PathBuf>,
    config: Option<&crate::config::ConfigSet>,
    query: &BranchQuery,
) -> Result<BranchDetail> {
    let kind = ref_kind_from_name(&listing.full_name);
    let remote = remote_name_from_ref(&listing.full_name, kind);
    let name = display_branch_name(&listing, kind);
    let is_current = current == Some(name.as_str());
    let (subject, committer) = if query.include_commit_details {
        commit_summary(repo, listing.oid)?
    } else {
        (None, None)
    };
    let upstream = if kind == RefKind::Head {
        upstream_for_local_branch(
            repo,
            &name,
            listing.oid,
            config,
            query.include_upstream_counts,
        )?
    } else {
        None
    };
    let worktree = worktrees.get(&name).cloned();
    Ok(BranchDetail {
        name,
        full_ref: listing.full_name,
        remote,
        oid: listing.oid,
        subject,
        committer,
        current: is_current,
        upstream,
        worktree,
    })
}

fn display_branch_name(listing: &RefListing, kind: RefKind) -> String {
    match kind {
        RefKind::Head => listing.short_name.clone(),
        RefKind::Remote => listing.short_name.clone(),
        _ => listing.short_name.clone(),
    }
}

fn remote_name_from_ref(full_ref: &str, kind: RefKind) -> Option<String> {
    if kind != RefKind::Remote {
        return None;
    }
    let rest = full_ref.strip_prefix("refs/remotes/")?;
    let (remote, _) = rest.split_once('/')?;
    Some(remote.to_owned())
}

fn commit_summary(repo: &Repository, oid: ObjectId) -> Result<(Option<String>, Option<String>)> {
    let data = repo.odb().read(&oid)?;
    if data.kind != ObjectKind::Commit {
        return Ok((None, None));
    }
    let subject = read_commit_subject_line(&data.data).ok();
    let committer = parse_commit(&data.data).ok().map(|c| c.committer);
    Ok((subject, committer))
}

fn upstream_for_local_branch(
    repo: &Repository,
    branch_short: &str,
    tip: ObjectId,
    config: Option<&crate::config::ConfigSet>,
    with_counts: bool,
) -> Result<Option<BranchUpstreamStatus>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let merge_key = format!("branch.{branch_short}.merge");
    let merge = match config.get(&merge_key) {
        Some(m) if !m.trim().is_empty() => m,
        _ => return Ok(None),
    };
    let remote = config
        .get(&format!("branch.{branch_short}.remote"))
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    let ref_name = abbreviate_ref_name(merge.trim());
    let tracking = upstream_tracking_full_ref(repo, branch_short);
    let resolved = tracking
        .as_ref()
        .and_then(|full| repo.refs().resolve(full).ok());
    if resolved.is_none() {
        return Ok(Some(BranchUpstreamStatus {
            ref_name,
            remote,
            gone: true,
            ahead: None,
            behind: None,
        }));
    }
    let (ahead, behind) = if with_counts {
        if let Some(upstream_oid) = resolved {
            let (a, b) = count_symmetric_ahead_behind(repo, tip, upstream_oid)?;
            (Some(a), Some(b))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    Ok(Some(BranchUpstreamStatus {
        ref_name,
        remote,
        gone: false,
        ahead,
        behind,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::porcelain::branch::{create_branch, list_branches};
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::repo::InitOptions;

    fn init() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
        (tmp, repo)
    }

    fn commit(repo: &Repository, message: &str) -> ObjectId {
        let req = CommitRequest {
            message: message.into(),
            author: "T <t@example.com> 1700000000 +0000".into(),
            committer: "T <t@example.com> 1700000000 +0000".into(),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        };
        create_commit(repo, &req, &mut NullProgress)
            .expect("commit")
            .oid
    }

    #[test]
    fn query_default_matches_list_branches_names() {
        let (_tmp, repo) = init();
        commit(&repo, "one");
        create_branch(&repo, "side").unwrap();
        let simple = list_branches(&repo).unwrap();
        let rich =
            query_branches(&repo, &BranchQuery::default(), &RefQueryOptions::default()).unwrap();
        assert_eq!(simple.current, rich.current);
        let simple_names: Vec<_> = simple.branches.iter().map(|b| b.name.as_str()).collect();
        let rich_names: Vec<_> = rich.branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(simple_names, rich_names);
    }

    #[test]
    fn query_includes_subject_when_requested() {
        let (_tmp, repo) = init();
        commit(&repo, "hello subject");
        let query = BranchQuery {
            include_commit_details: true,
            ..BranchQuery::default()
        };
        let out = query_branches(&repo, &query, &RefQueryOptions::default()).unwrap();
        let main = out.branches.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.subject.as_deref(), Some("hello subject"));
        assert!(main.committer.is_some());
    }
}

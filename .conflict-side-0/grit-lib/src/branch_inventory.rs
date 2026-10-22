//! Branch listing with upstream and worktree metadata ([`list_branches`]).

use std::collections::HashMap;
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::objects::{read_commit_subject_pretty, ObjectId, ObjectKind};
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};
use crate::refs::listing::{
    query_refs, RefKind, RefNamespace, RefQuery, RefQueryError, RefQueryOptions, SortKey,
};
use crate::repo::Repository;
use crate::rev_parse::abbreviate_ref_name;
use crate::state::{resolve_head_for_repository, HeadState};
use crate::worktree::list_worktrees;

/// Which branches to include in [`list_branches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchScope {
    /// Local branches only (`refs/heads/*`).
    #[default]
    Local,
    /// Remote-tracking branches only (`refs/remotes/*`).
    Remote,
    /// Local and remote-tracking branches.
    All,
}

/// How much object data [`list_branches`] loads per branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchDetail {
    /// Ref names and oids only; no commit objects read for branch fields.
    #[default]
    Names,
    /// Commit subjects, committer times, and upstream ahead/behind when configured.
    Full,
}

/// Query for [`list_branches`]: scope plus shared ref patterns, sort, and limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BranchListQuery {
    /// Local, remote, or both.
    pub scope: BranchScope,
    /// Pattern, sort, and limit (namespaces come from [`Self::scope`]).
    pub refs: RefQuery,
}

/// Upstream of a local branch when `branch.<name>.merge` / `.remote` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamStatus {
    /// Full upstream ref (`refs/remotes/...` or `refs/heads/...`).
    pub full_ref: String,
    /// Abbreviated upstream name (`origin/main`).
    pub short: String,
    /// Configured `branch.<name>.remote`.
    pub remote: String,
    /// Configured `branch.<name>.merge`.
    pub merge: String,
    /// Upstream ref is configured but does not resolve.
    pub gone: bool,
    /// Commits on the branch not on the upstream tip ([`BranchDetail::Full`] only).
    pub ahead: Option<usize>,
    /// Commits on the upstream not on the branch ([`BranchDetail::Full`] only).
    pub behind: Option<usize>,
}

/// One row from [`list_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSummary {
    /// Short display name (`main`, `origin/main`, …).
    pub name: String,
    /// Full ref name.
    pub full_ref: String,
    /// True for `refs/remotes/*`.
    pub is_remote: bool,
    /// Whether this branch is the current `HEAD` branch.
    pub current: bool,
    /// Tip object id.
    pub oid: ObjectId,
    /// Tip commit subject ([`BranchDetail::Full`], Git `%(contents:subject)`).
    pub subject: Option<String>,
    /// Tip committer time ([`BranchDetail::Full`]).
    pub committer_time: Option<OffsetDateTime>,
    /// Configured upstream for local branches.
    pub upstream: Option<UpstreamStatus>,
    /// Linked worktree path when this branch is checked out there (Git `worktreepath`).
    pub worktree: Option<PathBuf>,
}

/// Configured upstream for local branch `branch` (short name, no `refs/heads/`).
///
/// Ahead/behind counts are always computed when the upstream resolves.
///
/// # Errors
///
/// Propagates configuration, ref resolution, and rev-walk errors.
pub fn upstream_status(repo: &Repository, branch: &str) -> Result<Option<UpstreamStatus>> {
    let tip = repo
        .references()
        .resolve_if_exists(&format!("refs/heads/{branch}"))?;
    build_upstream_status(repo, branch, tip, BranchDetail::Full)
}

/// List branches matching `query` at the requested detail level.
///
/// [`BranchDetail::Names`] does not read commit objects for summary fields.
/// [`BranchDetail::Full`] loads commit headers in parallel and computes upstream
/// ahead/behind via [`crate::branch_tracking::TipComparison`]. Worktree paths are resolved once per call.
///
/// # Errors
///
/// Propagates ref listing, configuration, object read, and walk errors.
pub fn list_branches(
    repo: &Repository,
    query: &BranchListQuery,
    detail: BranchDetail,
) -> Result<Vec<BranchSummary>> {
    if detail == BranchDetail::Names && matches!(query.refs.sort.key, SortKey::Date) {
        return Err(Error::BranchListNamesDateSort);
    }
    let listings = query_refs(
        repo,
        &ref_query_for_branch_list(query),
        &ref_query_options(repo)?,
    )
    .map_err(ref_query_error)?;

    let current = match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    };

    let worktrees = branch_worktree_paths(repo)?;

    let commit_meta = if detail == BranchDetail::Full {
        let oids: Vec<ObjectId> = listings.iter().map(|l| l.oid).collect();
        Some(load_commit_metadata(repo, &oids)?)
    } else {
        None
    };

    let mut out = Vec::with_capacity(listings.len());
    for (idx, listing) in listings.into_iter().enumerate() {
        let is_remote = listing.kind == RefKind::Remote;
        let current = !is_remote && current.as_deref() == Some(listing.short_name.as_str());
        let (subject, committer_time) = commit_meta
            .as_ref()
            .and_then(|m| m.get(idx))
            .cloned()
            .unwrap_or((None, None));

        let upstream = if is_remote {
            None
        } else {
            build_upstream_status(repo, &listing.short_name, Some(listing.oid), detail)?
        };

        let worktree = if is_remote {
            None
        } else {
            worktrees.get(&listing.short_name).cloned()
        };

        out.push(BranchSummary {
            name: branch_display_name(query.scope, is_remote, &listing.short_name),
            full_ref: listing.full_name,
            is_remote,
            current,
            oid: listing.oid,
            subject,
            committer_time,
            upstream,
            worktree,
        });
    }
    Ok(out)
}

fn branch_display_name(scope: BranchScope, is_remote: bool, short_name: &str) -> String {
    if is_remote && scope == BranchScope::All {
        format!("remotes/{short_name}")
    } else {
        short_name.to_owned()
    }
}

fn ref_query_for_branch_list(query: &BranchListQuery) -> RefQuery {
    let mut refs = query.refs.clone();
    refs.namespaces = match query.scope {
        BranchScope::Local => vec![RefNamespace::Heads],
        BranchScope::Remote => vec![RefNamespace::Remotes],
        BranchScope::All => vec![RefNamespace::Heads, RefNamespace::Remotes],
    };
    refs
}

fn ref_query_options(repo: &Repository) -> Result<RefQueryOptions> {
    let config = repo.config()?;
    Ok(RefQueryOptions {
        versionsort_suffixes: config
            .get_all("versionsort.suffix")
            .iter()
            .map(|s| s.to_owned())
            .collect(),
        ignore_case: config
            .get_bool("core.ignorecase")
            .and_then(|r| r.ok())
            .unwrap_or(false),
    })
}

fn ref_query_error(err: RefQueryError) -> Error {
    match err {
        RefQueryError::Store(e) => e,
        RefQueryError::MissingObjectDatabase => {
            Error::Message("creator-date sort requires commit objects".into())
        }
        RefQueryError::MissingObject(oid) => Error::ObjectNotFound(oid.to_string()),
        RefQueryError::NotCommit(oid) => Error::Message(format!("not a commit: {oid}")),
    }
}

fn branch_worktree_paths(repo: &Repository) -> Result<HashMap<String, PathBuf>> {
    let mut map = HashMap::new();
    for entry in list_worktrees(repo)? {
        if let HeadState::Branch { short_name, .. } = &entry.head {
            map.entry(short_name.clone())
                .or_insert_with(|| entry.path.clone());
        }
    }
    Ok(map)
}

fn build_upstream_status(
    repo: &Repository,
    branch_short: &str,
    branch_tip: Option<ObjectId>,
    detail: BranchDetail,
) -> Result<Option<UpstreamStatus>> {
    let config = repo.config()?;
    let remote_key = format!("branch.{branch_short}.remote");
    let merge_key = format!("branch.{branch_short}.merge");
    let Some(full_ref) = crate::branch_tracking::branch_upstream_full_ref(&config, branch_short)
    else {
        return Ok(None);
    };
    let remote = config.get(&remote_key).unwrap_or_else(|| ".".to_owned());
    let merge = config
        .get(&merge_key)
        .unwrap_or_else(|| format!("refs/heads/{branch_short}"));
    let resolved = repo.references().resolve_if_exists(&full_ref)?;
    let gone = resolved.is_none();
    let short = abbreviate_ref_name(&full_ref);
    let (ahead, behind) = if detail == BranchDetail::Full && !gone {
        if let (Some(tip), Some(up_oid)) = (branch_tip, resolved) {
            let cmp = crate::branch_tracking::TipComparison::compute(repo, tip, up_oid, 0)?;
            (Some(cmp.ahead), Some(cmp.behind))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    Ok(Some(UpstreamStatus {
        full_ref,
        short,
        remote,
        merge,
        gone,
        ahead,
        behind,
    }))
}

fn load_commit_metadata(
    repo: &Repository,
    oids: &[ObjectId],
) -> Result<Vec<(Option<String>, Option<OffsetDateTime>)>> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let config = repo.config()?;
    let par_opts = repo
        .thread_policy()
        .resolve(ParallelOp::DiffContent, &config);
    let total_bytes = oids.len().saturating_mul(512);
    let rows = try_par_map(oids, par_opts, total_bytes, |oid| {
        read_commit_meta(repo, oid)
    })
    .map_err(|e| match e {
        ParallelHashError::Task(err) => err,
    })?;
    Ok(rows)
}

fn read_commit_meta(
    repo: &Repository,
    oid: &ObjectId,
) -> Result<(Option<String>, Option<OffsetDateTime>)> {
    let obj = repo.odb().read(oid)?;
    if obj.kind != ObjectKind::Commit {
        return Ok((None, None));
    }
    let subject = read_commit_subject_pretty(&obj.data).ok();
    let committer_time = crate::objects::CommitRef::parse(&obj.data)
        .ok()
        .and_then(|cref| {
            OffsetDateTime::from_unix_timestamp(cref.committer_unix_seconds_for_ordering()).ok()
        });
    Ok((subject, committer_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_display_name_matches_git_branch_a_for_remotes() {
        assert_eq!(
            branch_display_name(BranchScope::All, true, "origin/main"),
            "remotes/origin/main"
        );
        assert_eq!(
            branch_display_name(BranchScope::Remote, true, "origin/main"),
            "origin/main"
        );
        assert_eq!(branch_display_name(BranchScope::All, false, "main"), "main");
    }
}

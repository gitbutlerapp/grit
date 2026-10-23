//! [`RepositoryRemotes::inspect`](crate::repo_handles::RepositoryRemotes::inspect) types and logic.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::refs;
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::remote::{HttpClientFactory, ListRefsOptions, Remote, RemoteError, RemoteRef};
use crate::repo::Repository;
use crate::repo_handles::has_remote_section;
use crate::transfer::{match_positive, ref_excluded, tracking_refs_gone_on_remote};

/// Options for [`RepositoryRemotes::inspect`](crate::repo_handles::RepositoryRemotes::inspect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectOptions {
    /// When `true`, contact the remote (or read its git dir) to classify branch states.
    pub query: bool,
    /// Maximum number of entries in [`RemoteSummary::branches`] (`None` = unlimited).
    pub limit: Option<usize>,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            query: true,
            limit: None,
        }
    }
}

/// Whether a remote branch is tracked locally, new on the remote, or stale locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RemoteBranchStateKind {
    /// A remote-tracking ref exists for this branch.
    Tracked,
    /// The branch exists on the remote but not as a local tracking ref.
    New,
    /// A local tracking ref exists but the branch is gone on the remote.
    Stale,
}

/// One remote branch and how it relates to local tracking refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranchState {
    /// Short branch name (the segment under `refs/heads/`).
    pub name: String,
    /// Classification relative to local tracking refs.
    pub state: RemoteBranchStateKind,
}

/// A local branch configured to pull from a remote via `branch.*` config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePullBranch {
    /// Local branch short name (`branch.<local>.remote` matches the remote).
    pub local: String,
    /// Value of `branch.<local>.merge` (typically `refs/heads/<branch>`).
    pub merges: String,
}

/// Result of inspecting one configured remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSummary {
    /// Remote name (config subsection).
    pub name: String,
    /// Fetch URL after config rewrite.
    pub fetch_url: String,
    /// Push URL (`pushurl` when set, else fetch URL).
    pub push_url: String,
    /// Default branch on the remote (`HEAD` symref target), when known.
    pub head_branch: Option<String>,
    /// Remote branches and their tracking state (bounded by [`InspectOptions::limit`]).
    pub branches: Vec<RemoteBranchState>,
    /// Local branches with `branch.<name>.remote` set to this remote.
    pub pull: Vec<RemotePullBranch>,
}

struct FetchRefspecs {
    positive: Vec<RefspecItem>,
    negative: Vec<RefspecItem>,
}

impl FetchRefspecs {
    fn load(config: &ConfigSet, remote_name: &str) -> Self {
        let (negative, positive) = config
            .get_all(&format!("remote.{remote_name}.fetch"))
            .iter()
            .filter_map(|spec| parse_fetch_refspec(spec).ok())
            .partition(|item| item.negative);
        Self { positive, negative }
    }
}

/// Inspect remote `name` and return URLs, default branch, branch states, and pull config.
///
/// # Errors
///
/// - [`RemoteError::UnknownRemote`] when no `remote.<name>.*` section exists.
/// - [`RemoteError::NoUrl`] when the remote has no URL.
/// - Propagates transport, config, and ref I/O failures as [`RemoteError::Library`].
pub fn inspect_remote(
    repo: &Repository,
    name: &str,
    options: &InspectOptions,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<RemoteSummary, RemoteError> {
    let config = repo.config().map_err(RemoteError::Library)?;
    if !has_remote_section(config.as_ref(), name) {
        return Err(RemoteError::UnknownRemote(name.to_owned()));
    }
    let remote = Remote::from_config(config.as_ref(), name)?;
    let fetch_url = remote.fetch_url().to_url_string();
    let push_url = remote.push_url().to_url_string();
    let git_dir = repo.git_dir();

    let (head_branch, branches) = if options.query {
        let list_opts = ListRefsOptions {
            heads: true,
            symrefs: true,
            ..Default::default()
        };
        let advertised = remote.list_refs(Some(repo), &list_opts, http_factory)?;
        let head_branch = head_branch_from_advertisement(&advertised);
        let branches = branch_states_from_query(
            git_dir,
            name,
            &FetchRefspecs::load(config.as_ref(), name),
            &advertised,
        )?;
        (head_branch, branches)
    } else {
        let head_branch = local_remote_head_branch(git_dir, name)?;
        let branches = local_tracking_branch_states(git_dir, name);
        (head_branch, branches)
    };

    let branches = apply_branch_limit(branches, options.limit);
    let pull = collect_pull_branches(config.as_ref(), name);

    Ok(RemoteSummary {
        name: name.to_owned(),
        fetch_url,
        push_url,
        head_branch,
        branches,
        pull,
    })
}

fn head_branch_from_advertisement(refs: &[RemoteRef]) -> Option<String> {
    refs.iter()
        .find(|r| r.name == "HEAD")
        .and_then(|r| r.symref_target.as_ref())
        .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned())
}

fn local_remote_head_branch(
    git_dir: &std::path::Path,
    remote_name: &str,
) -> Result<Option<String>> {
    let head_ref = format!("refs/remotes/{remote_name}/HEAD");
    let Some(target) = refs::read_symbolic_ref(git_dir, &head_ref)? else {
        return Ok(None);
    };
    Ok(target
        .strip_prefix(&format!("refs/remotes/{remote_name}/"))
        .or_else(|| target.strip_prefix("refs/heads/"))
        .map(str::to_owned))
}

fn local_tracking_branch_states(
    git_dir: &std::path::Path,
    remote_name: &str,
) -> Vec<RemoteBranchState> {
    local_remote_branch_names(git_dir, remote_name)
        .into_iter()
        .map(|name| RemoteBranchState {
            name,
            state: RemoteBranchStateKind::Tracked,
        })
        .collect()
}

fn local_remote_branch_names(git_dir: &std::path::Path, remote_name: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{remote_name}/");
    let Ok(entries) = refs::list_refs(git_dir, &prefix) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter_map(|(refname, _)| {
            if refname == format!("{prefix}HEAD") {
                return None;
            }
            refname.strip_prefix(&prefix).map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

fn remote_head_branch_names(
    refspecs: &FetchRefspecs,
    remote_refs: &[RemoteRef],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for entry in remote_refs {
        if !entry.name.starts_with("refs/heads/") {
            continue;
        }
        if ref_excluded(&entry.name, &refspecs.negative) {
            continue;
        }
        if match_positive(&entry.name, &refspecs.positive).is_some() {
            if let Some(short) = entry.name.strip_prefix("refs/heads/") {
                names.insert(short.to_owned());
            }
        }
    }
    names
}

fn branch_states_from_query(
    git_dir: &std::path::Path,
    remote_name: &str,
    refspecs: &FetchRefspecs,
    advertised: &[RemoteRef],
) -> Result<Vec<RemoteBranchState>> {
    let remote_heads: BTreeSet<String> = remote_head_branch_names(refspecs, advertised);
    let local_names: BTreeSet<String> = local_remote_branch_names(git_dir, remote_name)
        .into_iter()
        .collect();

    let remote_pairs: Vec<(String, ObjectId)> =
        advertised.iter().map(|r| (r.name.clone(), r.oid)).collect();
    let stale_full = tracking_refs_gone_on_remote(git_dir, &refspecs.positive, &remote_pairs)?;
    let stale_names: BTreeSet<String> = stale_full
        .into_iter()
        .filter_map(|(full, _)| tracking_ref_to_branch(&full, remote_name))
        .collect();

    let mut by_name: BTreeMap<String, RemoteBranchStateKind> = BTreeMap::new();
    for name in &remote_heads {
        let state = if local_names.contains(name) {
            RemoteBranchStateKind::Tracked
        } else {
            RemoteBranchStateKind::New
        };
        by_name.insert(name.clone(), state);
    }
    for name in stale_names {
        by_name
            .entry(name)
            .or_insert(RemoteBranchStateKind::Stale);
    }
    Ok(by_name
        .into_iter()
        .map(|(name, state)| RemoteBranchState { name, state })
        .collect())
}

fn tracking_ref_to_branch(full_ref: &str, remote_name: &str) -> Option<String> {
    let prefix = format!("refs/remotes/{remote_name}/");
    full_ref.strip_prefix(&prefix).map(str::to_owned)
}

fn apply_branch_limit(
    mut branches: Vec<RemoteBranchState>,
    limit: Option<usize>,
) -> Vec<RemoteBranchState> {
    if let Some(n) = limit {
        branches.truncate(n);
    }
    branches
}

fn collect_pull_branches(config: &ConfigSet, remote_name: &str) -> Vec<RemotePullBranch> {
    let mut by_branch: BTreeMap<String, RemotePullBranch> = BTreeMap::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("branch.") else {
            continue;
        };
        let Some((branch, key)) = rest.rsplit_once('.') else {
            continue;
        };
        if key != "remote" || entry.value.as_deref() != Some(remote_name) {
            continue;
        }
        let merge = config
            .get(&format!("branch.{branch}.merge"))
            .unwrap_or_default();
        by_branch.insert(
            branch.to_owned(),
            RemotePullBranch {
                local: branch.to_owned(),
                merges: merge,
            },
        );
    }
    by_branch.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use crate::repo::{InitOptions, Repository};

    fn config_set(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(std::path::Path::new("cfg"), text, ConfigScope::Local)
            .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn collect_pull_branches_reads_branch_config() {
        let set = config_set(
            "[remote \"origin\"]\n\turl = /tmp/r.git\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n[branch \"dev\"]\n\tremote = upstream\n\tmerge = refs/heads/dev\n[branch \"feature\"]\n\tremote = origin\n\tmerge = refs/heads/feature\n",
        );
        let pull = collect_pull_branches(&set, "origin");
        assert_eq!(pull.len(), 2);
        assert!(pull
            .iter()
            .any(|p| p.local == "main" && p.merges == "refs/heads/main"));
        assert!(pull
            .iter()
            .any(|p| p.local == "feature" && p.merges == "refs/heads/feature"));
    }

    #[test]
    fn apply_branch_limit_truncates() {
        let branches = vec![
            RemoteBranchState {
                name: "a".into(),
                state: RemoteBranchStateKind::Tracked,
            },
            RemoteBranchState {
                name: "b".into(),
                state: RemoteBranchStateKind::New,
            },
        ];
        let limited = apply_branch_limit(branches, Some(1));
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].name, "a");
    }

    #[test]
    fn inspect_unknown_remote_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let err = inspect_remote(&repo, "missing", &InspectOptions::default(), None)
            .expect_err("unknown");
        assert!(matches!(err, RemoteError::UnknownRemote(name) if name == "missing"));
    }
}

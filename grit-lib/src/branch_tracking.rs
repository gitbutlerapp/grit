//! Comparison bases for a branch (its upstream or the repository's target branch) and
//! remote-tracking ref updates after a push.

use std::path::Path;

use crate::config::ConfigSet;
use crate::environment::Environment;
use crate::error::Result;
use crate::merge_base::ahead_behind_of_target_commits;
use crate::objects::ObjectId;
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::repo::Repository;
use crate::rev_parse::abbreviate_ref_name;
use crate::transfer::{match_positive, ref_excluded};

/// The `remote.<name>.fetch` refspecs of one remote, split by polarity.
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

    /// The local tracking ref `remote_ref` maps to, or `None` when no positive refspec maps it
    /// or a negative one excludes it. With no positive refspecs at all, `refs/heads/<branch>`
    /// falls back to `refs/remotes/<remote>/<branch>`.
    fn tracking_ref(&self, remote_name: &str, remote_ref: &str) -> Option<String> {
        if ref_excluded(remote_ref, &self.negative) {
            return None;
        }
        match match_positive(remote_ref, &self.positive) {
            Some(dst) => dst,
            None if self.positive.is_empty() => remote_ref
                .strip_prefix("refs/heads/")
                .map(|branch| format!("refs/remotes/{remote_name}/{branch}")),
            None => None,
        }
    }
}

/// Apply remote-tracking ref updates for every successfully pushed ref in `results`.
///
/// Refs reported as [`PushRefStatus::Ok`] or [`PushRefStatus::UpToDate`] are mapped through
/// the remote's `remote.<name>.fetch` refspecs and updated in one ref-store batch (matching
/// Git `transport_update_tracking_ref`); a pushed deletion removes the tracking ref. Path-like
/// remote names (containing `/` or starting with `.`) and an unreadable config skip tracking
/// updates entirely.
///
/// # Parameters
///
/// - `git_dir`: local repository `.git` directory.
/// - `remote_name`: short remote name (e.g. `origin`).
/// - `results`: per-ref push outcomes reported by the remote.
///
/// # Errors
///
/// Propagates errors from [`refs::commit_ref_store_batch`].
pub fn apply_push_remote_tracking_updates(
    git_dir: &Path,
    remote_name: &str,
    results: &[PushRefResult],
) -> Result<()> {
    if remote_name.contains('/') || remote_name.starts_with('.') {
        return Ok(());
    }
    let Ok(config) = ConfigSet::load(&Environment::empty(), Some(git_dir), true) else {
        return Ok(());
    };
    let refspecs = FetchRefspecs::load(&config, remote_name);
    let batch: Vec<RefUpdate> = results
        .iter()
        .filter(|result| matches!(result.status, PushRefStatus::Ok | PushRefStatus::UpToDate))
        .filter_map(|result| {
            let name = refspecs.tracking_ref(remote_name, &result.remote_ref)?;
            let current = refs::resolve_ref(git_dir, &name).ok();
            let new_value = match result.new_oid {
                Some(oid) if current == Some(oid) => return None,
                Some(oid) => Some(RawRef::Direct(oid)),
                None if current.is_none() => return None,
                None => None,
            };
            Some(RefUpdate {
                name,
                new_value,
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
        })
        .collect();
    if !batch.is_empty() {
        refs::commit_ref_store_batch(git_dir, &batch)?;
    }
    Ok(())
}

/// Full ref for the configured upstream of `branch_short` (`refs/remotes/...` or `refs/heads/...`).
#[must_use]
pub fn upstream_tracking_full_ref(repo: &Repository, branch_short: &str) -> Option<String> {
    let config = repo.config().ok()?;
    let remote = config.get(&format!("branch.{branch_short}.remote"))?;
    let merge = config.get(&format!("branch.{branch_short}.merge"))?;
    if remote == "." {
        let m = merge.trim();
        if m.starts_with("refs/") {
            Some(m.to_owned())
        } else {
            Some(format!("refs/heads/{m}"))
        }
    } else {
        let mb = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
        Some(format!("refs/remotes/{remote}/{mb}"))
    }
}

/// Where a [`ComparisonBase`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseSource {
    /// The branch's configured upstream (`branch.<name>.remote` / `branch.<name>.merge`).
    Upstream,
    /// The repository's target branch (see [`target_branch`]).
    Target,
}

/// A resolved branch that another branch is measured against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparisonBase {
    /// Short display name: the full ref abbreviated (`origin/main`) for an upstream, or the
    /// candidate as configured or listed in [`DEFAULT_TARGET_CANDIDATES`] for a target.
    pub name: String,
    /// Full ref the base resolved through (for example `refs/remotes/origin/main`).
    pub full_ref: String,
    /// Commit the base points at.
    pub oid: ObjectId,
    /// Whether the base is an upstream or the target branch.
    pub source: BaseSource,
}

/// Trunk names tried, in order, after `target.branch` when looking for the target branch.
pub const DEFAULT_TARGET_CANDIDATES: [&str; 4] = ["origin/master", "origin/main", "master", "main"];

/// Resolve `refname` through the repository's ref store; refs that are missing or cannot be
/// resolved (dangling symrefs, unreadable files) count as absent.
fn resolve_optional(repo: &Repository, refname: &str) -> Option<ObjectId> {
    repo.refs().resolve(refname).ok()
}

/// Full refs to try for one target-branch candidate, most specific first.
fn target_candidate_refnames(candidate: &str) -> Vec<String> {
    if candidate.starts_with("refs/") || candidate == "HEAD" {
        return vec![candidate.to_owned()];
    }
    if let Some(remote_branch) = candidate.strip_prefix("origin/") {
        return vec![
            format!("refs/remotes/origin/{remote_branch}"),
            format!("refs/heads/{candidate}"),
        ];
    }
    vec![
        format!("refs/heads/{candidate}"),
        format!("refs/remotes/{candidate}"),
    ]
}

/// The repository's target branch: the trunk the current branch is measured against.
///
/// Tries the `target.branch` config value first (when set and non-blank), then each of
/// [`DEFAULT_TARGET_CANDIDATES`]. A candidate is a full ref (`refs/...` or `HEAD`), an
/// `origin/<branch>` name (tried as `refs/remotes/origin/<branch>`, then as a local branch of
/// that name), or a short name (tried as a local branch, then as a remote-tracking ref).
/// Refs are resolved through the repository's ref store, so every ref backend is honoured.
///
/// # Parameters
///
/// - `repo`: the repository to search.
///
/// # Returns
///
/// The first candidate that resolves, with [`BaseSource::Target`], or `None` when none does.
///
/// # Errors
///
/// Returns an error when the repository configuration cannot be loaded.
pub fn target_branch(repo: &Repository) -> Result<Option<ComparisonBase>> {
    let config = repo.config()?;
    let configured = config
        .get("target.branch")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let candidates = configured
        .into_iter()
        .chain(DEFAULT_TARGET_CANDIDATES.iter().map(|c| (*c).to_owned()));
    for candidate in candidates {
        for full_ref in target_candidate_refnames(&candidate) {
            if let Some(oid) = resolve_optional(repo, &full_ref) {
                return Ok(Some(ComparisonBase {
                    name: candidate,
                    full_ref,
                    oid,
                    source: BaseSource::Target,
                }));
            }
        }
    }
    Ok(None)
}

/// The configured upstream of local branch `branch_short`, when it is set and resolves.
///
/// The upstream ref comes from [`upstream_tracking_full_ref`] and is resolved through the
/// repository's ref store.
///
/// # Parameters
///
/// - `repo`: the repository.
/// - `branch_short`: local branch name without `refs/heads/`.
///
/// # Returns
///
/// The upstream with [`BaseSource::Upstream`], or `None` when no upstream is configured or
/// its ref does not resolve (for example before the first fetch).
#[must_use]
pub fn upstream_branch(repo: &Repository, branch_short: &str) -> Option<ComparisonBase> {
    let full_ref = upstream_tracking_full_ref(repo, branch_short)?;
    let oid = resolve_optional(repo, &full_ref)?;
    Some(ComparisonBase {
        name: abbreviate_ref_name(&full_ref),
        full_ref,
        oid,
        source: BaseSource::Upstream,
    })
}

/// The base branch `branch_short` is compared against: its [`upstream_branch`] when that
/// resolves, otherwise the repository's [`target_branch`].
///
/// # Parameters
///
/// - `repo`: the repository.
/// - `branch_short`: local branch name without `refs/heads/`.
///
/// # Errors
///
/// Returns an error when the repository configuration cannot be loaded.
pub fn comparison_base(repo: &Repository, branch_short: &str) -> Result<Option<ComparisonBase>> {
    match upstream_branch(repo, branch_short) {
        Some(upstream) => Ok(Some(upstream)),
        None => target_branch(repo),
    }
}

/// How a branch tip relates to its base, from symmetric ahead/behind counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Neither side has commits the other lacks.
    Even,
    /// Only the branch has commits the base lacks.
    Ahead,
    /// Only the base has commits the branch lacks.
    Behind,
    /// Both sides have commits the other lacks.
    Diverged,
}

impl Standing {
    /// Classify symmetric counts: `ahead` commits only on the branch, `behind` only on the base.
    #[must_use]
    pub fn from_counts(ahead: usize, behind: usize) -> Self {
        match (ahead, behind) {
            (0, 0) => Self::Even,
            (_, 0) => Self::Ahead,
            (0, _) => Self::Behind,
            _ => Self::Diverged,
        }
    }
}

/// Symmetric comparison of a branch tip with a base commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TipComparison {
    /// Commits reachable from the tip but not from the base (full count).
    pub ahead: usize,
    /// Commits reachable from the base but not from the tip (full count).
    pub behind: usize,
    /// Newest of the `ahead` commits (newest first), at most the requested limit.
    pub newest_ahead: Vec<ObjectId>,
}

impl TipComparison {
    /// Compare `tip` with `base`, matching `git rev-list --left-right --count tip...base` for
    /// the counts and keeping at most `retain_newest_ahead` of the ahead commits.
    ///
    /// The walk streams commits instead of materializing ancestor closures, and skips a side
    /// entirely when one tip is an ancestor of the other.
    ///
    /// # Parameters
    ///
    /// - `repo`: the repository holding both commits.
    /// - `tip`: the branch tip.
    /// - `base`: the commit it is compared against.
    /// - `retain_newest_ahead`: how many of the newest ahead commits to return.
    ///
    /// # Errors
    ///
    /// Propagates object read and parse errors from the commit walk.
    pub fn compute(
        repo: &Repository,
        tip: ObjectId,
        base: ObjectId,
        retain_newest_ahead: usize,
    ) -> Result<Self> {
        let (ahead, behind, newest_ahead) =
            ahead_behind_of_target_commits(repo, tip, base, retain_newest_ahead)?;
        Ok(Self {
            ahead,
            behind,
            newest_ahead,
        })
    }

    /// How the tip stands relative to the base.
    #[must_use]
    pub fn standing(&self) -> Standing {
        Standing::from_counts(self.ahead, self.behind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refspec::RefspecError;

    #[test]
    fn standing_from_counts_covers_every_quadrant() {
        assert_eq!(Standing::from_counts(0, 0), Standing::Even);
        assert_eq!(Standing::from_counts(3, 0), Standing::Ahead);
        assert_eq!(Standing::from_counts(0, 2), Standing::Behind);
        assert_eq!(Standing::from_counts(1, 1), Standing::Diverged);
    }

    #[test]
    fn target_candidates_expand_to_full_refs() {
        assert_eq!(
            target_candidate_refnames("refs/heads/dev"),
            vec!["refs/heads/dev".to_owned()]
        );
        assert_eq!(target_candidate_refnames("HEAD"), vec!["HEAD".to_owned()]);
        assert_eq!(
            target_candidate_refnames("origin/main"),
            vec![
                "refs/remotes/origin/main".to_owned(),
                "refs/heads/origin/main".to_owned(),
            ]
        );
        assert_eq!(
            target_candidate_refnames("upstream/dev"),
            vec![
                "refs/heads/upstream/dev".to_owned(),
                "refs/remotes/upstream/dev".to_owned(),
            ]
        );
    }

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn refspecs(specs: &[&str]) -> std::result::Result<FetchRefspecs, RefspecError> {
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        for spec in specs {
            let item = parse_fetch_refspec(spec)?;
            if item.negative {
                negative.push(item);
            } else {
                positive.push(item);
            }
        }
        Ok(FetchRefspecs { positive, negative })
    }

    #[test]
    fn tracking_ref_follows_fetch_refspecs() -> TestResult {
        let specs = refspecs(&[
            "+refs/heads/*:refs/remotes/upstream/*",
            "^refs/heads/secret",
        ])?;
        assert_eq!(
            specs.tracking_ref("origin", "refs/heads/main").as_deref(),
            Some("refs/remotes/upstream/main")
        );
        assert_eq!(specs.tracking_ref("origin", "refs/heads/secret"), None);
        assert_eq!(specs.tracking_ref("origin", "refs/tags/v1"), None);
        Ok(())
    }

    #[test]
    fn tracking_ref_without_fetch_refspecs_maps_branches_only() -> TestResult {
        let specs = refspecs(&[])?;
        assert_eq!(
            specs.tracking_ref("origin", "refs/heads/topic").as_deref(),
            Some("refs/remotes/origin/topic")
        );
        assert_eq!(specs.tracking_ref("origin", "refs/tags/v1"), None);

        let only_negative = refspecs(&["^refs/heads/wip"])?;
        assert_eq!(only_negative.tracking_ref("origin", "refs/heads/wip"), None);
        assert_eq!(
            only_negative
                .tracking_ref("origin", "refs/heads/main")
                .as_deref(),
            Some("refs/remotes/origin/main")
        );
        Ok(())
    }

    #[test]
    fn fetch_refspecs_load_reads_one_remote() -> TestResult {
        let mut config = ConfigSet::new();
        config
            .add_command_override("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
        config.add_command_override("remote.origin.fetch", "^refs/heads/wip")?;
        config.add_command_override("remote.other.fetch", "+refs/heads/*:refs/remotes/other/*")?;
        let specs = FetchRefspecs::load(&config, "origin");
        assert_eq!(specs.positive.len(), 1);
        assert_eq!(specs.negative.len(), 1);
        assert_eq!(
            specs.tracking_ref("origin", "refs/heads/main").as_deref(),
            Some("refs/remotes/origin/main")
        );
        Ok(())
    }
}

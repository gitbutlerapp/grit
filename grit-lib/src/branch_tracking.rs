//! Comparison bases for a branch (its upstream or the repository's target branch) and
//! remote-tracking ref updates after a push.

use std::path::Path;

use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::environment::Environment;
use crate::error::Result;
use crate::fetch_head::merge_object_ids_hex;
use crate::merge_base::ahead_behind_of_target_commits;
use crate::objects::ObjectId;
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::repo::Repository;
use crate::rev_parse::abbreviate_ref_name;
use crate::state::{HeadState, InProgressOperation};
use crate::transfer::{match_positive, ref_excluded};

/// The `remote.<name>.fetch` refspecs of one remote, split by polarity.
struct FetchRefspecs {
    positive: Vec<RefspecItem>,
    negative: Vec<RefspecItem>,
}

/// Whether `tracking_ref` is the destination of a positive fetch refspec for its remote.
///
/// Returns `false` when `tracking_ref` is not under `refs/remotes/`.
pub fn remote_tracking_ref_is_mapped(repo: &Repository, tracking_ref: &str) -> Result<bool> {
    let Some(rest) = tracking_ref.strip_prefix("refs/remotes/") else {
        return Ok(false);
    };
    let Some((remote, branch)) = rest.split_once('/') else {
        return Ok(false);
    };
    let config = repo.config()?;
    let refspecs = FetchRefspecs::load(&config, remote);
    let remote_ref = format!("refs/heads/{branch}");
    Ok(refspecs.tracking_ref(remote, &remote_ref).as_deref() == Some(tracking_ref))
}

/// Map a resolved upstream ref to `branch.<name>.remote` / `.merge` values.
#[must_use]
pub fn upstream_config_for_branch_ref(upstream_full: &str) -> Option<(String, String)> {
    if let Some(rest) = upstream_full.strip_prefix("refs/remotes/") {
        let (remote, branch) = rest.split_once('/')?;
        return Some((remote.to_owned(), format!("refs/heads/{branch}")));
    }
    upstream_full
        .strip_prefix("refs/heads/")
        .map(|branch| (".".to_owned(), format!("refs/heads/{branch}")))
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
///
/// Both `branch.<name>.remote` and `branch.<name>.merge` must be set. For the local remote `.`
/// the merge ref is itself the upstream; otherwise it is mapped through
/// `remote.<remote>.fetch` (with no positive refspecs, `refs/heads/<branch>` maps to
/// `refs/remotes/<remote>/<branch>`). `None` when the refspecs do not store it locally.
#[must_use]
pub fn upstream_tracking_full_ref(repo: &Repository, branch_short: &str) -> Option<String> {
    let config = repo.config().ok()?;
    let remote = config.get(&format!("branch.{branch_short}.remote"))?;
    let merge = config.get(&format!("branch.{branch_short}.merge"))?;
    upstream_ref_for_merge(&config, remote.trim(), &merge)
}

/// Same as [`upstream_tracking_full_ref`] with an already-loaded config (branch inventory).
#[must_use]
pub(crate) fn branch_upstream_full_ref(config: &ConfigSet, branch_short: &str) -> Option<String> {
    let remote = config.get(&format!("branch.{branch_short}.remote"))?;
    let merge = config.get(&format!("branch.{branch_short}.merge"))?;
    upstream_ref_for_merge(config, remote.trim(), &merge)
}

/// The local ref that holds `merge` (a `branch.<name>.merge` value) of `remote`.
///
/// For the local remote `.` the merge ref is itself a local ref. Otherwise the remote ref
/// (`refs/heads/<merge>` when `merge` is a short name) is mapped through
/// `remote.<remote>.fetch`; with no positive fetch refspecs, `refs/heads/<branch>` maps to
/// `refs/remotes/<remote>/<branch>`. Returns `None` when the refspecs do not fetch the ref
/// into a remote-tracking ref.
pub(crate) fn upstream_ref_for_merge(
    config: &ConfigSet,
    remote: &str,
    merge: &str,
) -> Option<String> {
    let merge = merge.trim();
    let merge_ref = if merge.starts_with("refs/") {
        merge.to_owned()
    } else {
        format!("refs/heads/{merge}")
    };
    if remote == "." {
        return Some(merge_ref);
    }
    FetchRefspecs::load(config, remote).tracking_ref(remote, &merge_ref)
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
    comparison_base_from_full_ref(repo, &full_ref, BaseSource::Upstream)
}

fn fetch_head_integration_name(content: &str, remote_name: &str) -> String {
    for line in content.lines() {
        if let Some(start) = line.find("branch '") {
            let rest = &line[start + 8..];
            if let Some(end) = rest.find('\'') {
                let branch = &rest[..end];
                return format!("{remote_name}/{branch}");
            }
        }
    }
    format!("{remote_name} (FETCH_HEAD)")
}

/// For-merge tip recorded in [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) after an explicit fetch.
#[must_use]
pub fn pull_upstream_from_fetch_head(
    repo: &Repository,
    remote_name: &str,
) -> Option<ComparisonBase> {
    let content = std::fs::read_to_string(repo.git_dir().join("FETCH_HEAD")).ok()?;
    let hex_ids = merge_object_ids_hex(&content);
    let first = hex_ids.first()?;
    let oid = ObjectId::from_hex(first).ok()?;
    let name = fetch_head_integration_name(&content, remote_name);
    Some(ComparisonBase {
        name,
        full_ref: "FETCH_HEAD".to_owned(),
        oid,
        source: BaseSource::Upstream,
    })
}

/// Commit [`crate::porcelain::pull::pull`] integrates after fetch.
///
/// Explicit command-line refspecs use [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion);
/// otherwise the configured upstream of `branch_short` is used.
#[must_use]
pub fn resolve_pull_integration_base(
    repo: &Repository,
    branch_short: &str,
    remote_name: &str,
    explicit_fetch: bool,
) -> Option<ComparisonBase> {
    if explicit_fetch {
        if let Some(base) = pull_upstream_from_fetch_head(repo, remote_name) {
            return Some(base);
        }
    }
    pull_upstream_branch(repo, branch_short, remote_name)
}

/// Upstream ref for [`crate::porcelain::pull::pull`].
///
/// When `branch.<name>.merge` is set, only that configured tracking ref is used; if it
/// does not resolve after fetch, returns `None` (no implicit fallback). When merge is
/// unset, uses Git's default `refs/remotes/<remote>/<branch_short>`.
#[must_use]
pub fn pull_upstream_branch(
    repo: &Repository,
    branch_short: &str,
    remote_name: &str,
) -> Option<ComparisonBase> {
    let config = repo.config().ok()?;
    let merge_key = format!("branch.{branch_short}.merge");
    if config.get(&merge_key).is_some() {
        let full_ref = upstream_tracking_full_ref(repo, branch_short)?;
        return comparison_base_from_full_ref(repo, &full_ref, BaseSource::Upstream);
    }
    let full_ref = format!("refs/remotes/{remote_name}/{branch_short}");
    comparison_base_from_full_ref(repo, &full_ref, BaseSource::Upstream)
}

fn comparison_base_from_full_ref(
    repo: &Repository,
    full_ref: &str,
    source: BaseSource,
) -> Option<ComparisonBase> {
    let oid = resolve_optional(repo, full_ref)?;
    Some(ComparisonBase {
        name: abbreviate_ref_name(full_ref),
        full_ref: full_ref.to_owned(),
        oid,
        source,
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

/// Options for [`BranchStanding::compute`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandingOptions {
    /// Maximum number of newest ahead commits to retain on each [`TrackedComparison`].
    pub newest_ahead_limit: usize,
    /// When `false`, only tip oids are compared; [`TrackedComparison::ahead`] and
    /// [`TrackedComparison::behind`] stay `None` and no rev-list walk runs.
    pub counts: bool,
}

/// Symmetric comparison of the current branch tip against one tracked ref (target or upstream).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedComparison {
    /// Short display name (`origin/main`, `main`, …).
    pub name: String,
    /// Full ref the comparison resolved through.
    pub full_ref: String,
    /// Tip oid of the tracked ref at comparison time.
    pub oid: ObjectId,
    /// Commits on the branch not reachable from the tracked tip; `None` when counts were skipped.
    pub ahead: Option<usize>,
    /// Commits on the tracked tip not reachable from the branch; `None` when counts were skipped.
    pub behind: Option<usize>,
    /// Newest commits among the ahead set (newest first), up to the requested limit.
    pub newest_ahead: Vec<ObjectId>,
}

/// How the status header summarizes branch vs target/upstream (before merge/rebase overrides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandingHeader {
    /// On a branch that has no commits yet.
    Unborn,
    /// HEAD is detached.
    Detached,
    /// HEAD could not be resolved.
    Invalid,
    /// On a branch with no target or upstream to compare against.
    NoTarget,
    /// Symmetric ahead/behind counts vs the primary comparison ref.
    Compared(Standing),
    /// Primary comparison ref resolves but ahead/behind counts were skipped.
    ComparedNoCounts,
}

/// Target and upstream standing for the current HEAD (status header / JSON).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchStanding {
    /// Current branch short name, or `None` when HEAD is detached or invalid.
    pub branch: Option<String>,
    /// Repository target branch comparison, when a target resolves.
    pub target: Option<TrackedComparison>,
    /// Configured upstream comparison, when it resolves and differs from [`Self::target`].
    pub upstream: Option<TrackedComparison>,
    /// Header classification for the primary comparison (target, else upstream).
    pub header: StandingHeader,
}

/// Status header after branch standing and in-progress operation overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusHeader {
    /// Normal branch vs target/upstream summary.
    Branch(StandingHeader),
    /// A merge is in progress (`MERGE_HEAD` exists).
    MergeInProgress,
    /// Another in-progress operation (rebase, cherry-pick, …).
    OtherInProgress,
}

impl StatusHeader {
    /// Choose the header line, preferring merge/rebase overrides over branch standing.
    #[must_use]
    pub fn resolve(
        standing: &BranchStanding,
        merge_in_progress: bool,
        operations: &[InProgressOperation],
    ) -> Self {
        if merge_in_progress || operations.contains(&InProgressOperation::Merge) {
            return Self::MergeInProgress;
        }
        if !operations.is_empty() {
            return Self::OtherInProgress;
        }
        Self::Branch(standing.header)
    }

    /// Short ref name for the primary header comparison (target, else upstream).
    #[must_use]
    pub fn primary_comparison_name(standing: &BranchStanding) -> Option<&str> {
        standing
            .target
            .as_ref()
            .map(|t| t.name.as_str())
            .or_else(|| standing.upstream.as_ref().map(|u| u.name.as_str()))
    }

    /// Configured upstream comparison when it differs from target, or the sole comparison ref.
    #[must_use]
    pub fn upstream_line(standing: &BranchStanding) -> Option<&TrackedComparison> {
        standing.upstream.as_ref()
    }

    /// Ref name for a `grit pull` hint: upstream when configured, otherwise target.
    #[must_use]
    pub fn pull_integration_ref(standing: &BranchStanding) -> Option<&str> {
        if let Some(up) = &standing.upstream {
            if up.ahead == Some(0) && up.behind.is_some_and(|b| b > 0) {
                return Some(up.name.as_str());
            }
            return None;
        }
        standing.target.as_ref().and_then(|t| {
            if t.ahead == Some(0) && t.behind.is_some_and(|b| b > 0) {
                Some(t.name.as_str())
            } else {
                None
            }
        })
    }
}

impl BranchStanding {
    /// Resolve target and upstream for `head` and compare the branch tip against each.
    ///
    /// When target and upstream resolve to the same ref, the symmetric walk runs once.
    /// With [`StandingOptions::counts`] set to `false`, only tip oids are compared and
    /// ahead/behind counts are omitted.
    ///
    /// # Errors
    ///
    /// Propagates configuration load errors and object read failures from the commit walk.
    pub fn compute(repo: &Repository, head: &HeadState, opts: StandingOptions) -> Result<Self> {
        match head {
            HeadState::Invalid => Ok(Self {
                branch: None,
                target: None,
                upstream: None,
                header: StandingHeader::Invalid,
            }),
            HeadState::Detached { .. } => Ok(Self {
                branch: None,
                target: None,
                upstream: None,
                header: StandingHeader::Detached,
            }),
            HeadState::Branch {
                short_name,
                oid: None,
                ..
            } => Ok(Self {
                branch: Some(short_name.clone()),
                target: None,
                upstream: None,
                header: StandingHeader::Unborn,
            }),
            HeadState::Branch {
                short_name, oid, ..
            } => {
                let target_base = target_branch(repo)?;
                let upstream_base = upstream_branch(repo, short_name);
                let tip = *oid;
                let (target, upstream) =
                    comparisons_for_branch(repo, tip, target_base, upstream_base, opts)?;
                let header = primary_standing_header(tip, target.as_ref(), upstream.as_ref(), opts);
                Ok(Self {
                    branch: Some(short_name.clone()),
                    target,
                    upstream,
                    header,
                })
            }
        }
    }
}

fn primary_standing_header(
    tip: Option<ObjectId>,
    target: Option<&TrackedComparison>,
    upstream: Option<&TrackedComparison>,
    opts: StandingOptions,
) -> StandingHeader {
    let Some(tracked) = target.or(upstream) else {
        return StandingHeader::NoTarget;
    };
    standing_header_for_tracked(tip, tracked, opts)
}

fn standing_header_for_tracked(
    tip: Option<ObjectId>,
    tracked: &TrackedComparison,
    opts: StandingOptions,
) -> StandingHeader {
    if !opts.counts {
        return if tip == Some(tracked.oid) {
            StandingHeader::Compared(Standing::Even)
        } else {
            StandingHeader::ComparedNoCounts
        };
    }
    StandingHeader::Compared(Standing::from_counts(
        tracked.ahead.unwrap_or(0),
        tracked.behind.unwrap_or(0),
    ))
}

fn comparisons_for_branch(
    repo: &Repository,
    tip: Option<ObjectId>,
    target_base: Option<ComparisonBase>,
    upstream_base: Option<ComparisonBase>,
    opts: StandingOptions,
) -> Result<(Option<TrackedComparison>, Option<TrackedComparison>)> {
    let Some(head_oid) = tip else {
        let target = target_base
            .map(|base| tracked_comparison(repo, tip, base, opts))
            .transpose()?;
        let upstream = upstream_base
            .filter(|base| target.as_ref().is_none_or(|t| t.full_ref != base.full_ref))
            .map(|base| tracked_comparison(repo, tip, base, opts))
            .transpose()?;
        return Ok((target, upstream));
    };

    let same_upstream_as_target = matches!(
        (target_base.as_ref(), upstream_base.as_ref()),
        (Some(t), Some(u)) if t.full_ref == u.full_ref
    );

    if same_upstream_as_target {
        let target = target_base
            .map(|base| tracked_comparison(repo, tip, base, opts))
            .transpose()?;
        return Ok((target, None));
    }

    if opts.counts {
        if let (Some(t_base), Some(u_base)) = (&target_base, &upstream_base) {
            let (target, upstream) = dual_tracked_comparisons(
                repo,
                head_oid,
                t_base.clone(),
                u_base.clone(),
                opts.newest_ahead_limit,
            )?;
            return Ok((Some(target), Some(upstream)));
        }
    }

    let target = target_base
        .map(|base| tracked_comparison(repo, tip, base, opts))
        .transpose()?;
    let upstream = upstream_base
        .filter(|base| target.as_ref().is_none_or(|t| t.full_ref != base.full_ref))
        .map(|base| tracked_comparison(repo, tip, base, opts))
        .transpose()?;
    Ok((target, upstream))
}

fn dual_tracked_comparisons(
    repo: &Repository,
    head: ObjectId,
    target_base: ComparisonBase,
    upstream_base: ComparisonBase,
    retain_newest_ahead: usize,
) -> Result<(TrackedComparison, TrackedComparison)> {
    let target_cmp = TipComparison::compute(repo, head, target_base.oid, retain_newest_ahead)?;
    let upstream_cmp = TipComparison::compute(repo, head, upstream_base.oid, 0)?;
    Ok((
        TrackedComparison {
            name: target_base.name,
            full_ref: target_base.full_ref,
            oid: target_base.oid,
            ahead: Some(target_cmp.ahead),
            behind: Some(target_cmp.behind),
            newest_ahead: target_cmp.newest_ahead,
        },
        TrackedComparison {
            name: upstream_base.name,
            full_ref: upstream_base.full_ref,
            oid: upstream_base.oid,
            ahead: Some(upstream_cmp.ahead),
            behind: Some(upstream_cmp.behind),
            newest_ahead: Vec::new(),
        },
    ))
}

fn tracked_comparison(
    repo: &Repository,
    tip: Option<ObjectId>,
    base: ComparisonBase,
    opts: StandingOptions,
) -> Result<TrackedComparison> {
    let ComparisonBase {
        name,
        full_ref,
        oid: base_oid,
        source: _,
    } = base;
    let (ahead, behind, newest_ahead) = match tip {
        None => (None, None, Vec::new()),
        Some(_head_oid) if !opts.counts => (None, None, Vec::new()),
        Some(head_oid) => {
            let comparison =
                TipComparison::compute(repo, head_oid, base_oid, opts.newest_ahead_limit)?;
            (
                Some(comparison.ahead),
                Some(comparison.behind),
                comparison.newest_ahead,
            )
        }
    };
    Ok(TrackedComparison {
        name,
        full_ref,
        oid: base_oid,
        ahead,
        behind,
        newest_ahead,
    })
}

/// Whether status should compute ahead/behind counts (`status.aheadBehind`, default true).
#[must_use]
pub fn status_ahead_behind_enabled(config: &ConfigSet) -> bool {
    let raw = config
        .get("status.aheadBehind")
        .or_else(|| config.get("status.aheadbehind"));
    match raw {
        None => true,
        Some(value) => parse_git_config_maybe_bool(&value).unwrap_or(true),
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

pub use crate::branch_inventory::{
    list_branches, upstream_status, BranchDetail, BranchListQuery, BranchScope, BranchSummary,
    UpstreamStatus,
};

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

    use crate::config::environment::RepositoryOptions;
    use crate::objects::ObjectId;
    use crate::repo::Repository;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn open(dir: &Path) -> Repository {
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
    }

    fn head_state(repo: &Repository) -> HeadState {
        crate::state::resolve_head(repo.git_dir()).expect("head")
    }

    fn commit(dir: &Path, file: &str, msg: &str) {
        std::fs::write(dir.join(file), msg.as_bytes()).expect("write");
        git(dir, &["add", file]);
        git(dir, &["commit", "-qm", msg]);
    }

    #[test]
    fn standing_target_and_upstream_distinct() {
        let tmp = TempDir::new().expect("tempdir");
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]);
        commit(&origin, "a", "m1");
        git(&origin, &["checkout", "-qb", "topic"]);
        commit(&origin, "b", "topic");
        git(
            tmp.path(),
            &["clone", "-q", "-b", "main", "origin", "clone"],
        );
        let clone = tmp.path().join("clone");
        git(&clone, &["checkout", "-q", "topic"]);
        git(&clone, &["branch", "-q", "develop", "main"]);
        git(
            &clone,
            &[
                "branch",
                "--set-upstream-to",
                "refs/remotes/origin/topic",
                "topic",
            ],
        );
        git(&clone, &["config", "target.branch", "develop"]);

        let repo = open(&clone);
        let standing = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        )
        .expect("standing");
        assert_eq!(standing.branch.as_deref(), Some("topic"));
        let target = standing.target.as_ref().expect("target");
        let upstream = standing.upstream.as_ref().expect("upstream");
        assert_eq!(target.name, "develop");
        assert_eq!(upstream.name, "origin/topic");
        assert_ne!(target.full_ref, upstream.full_ref);
    }

    #[test]
    fn standing_upstream_equals_target_computed_once() {
        let tmp = TempDir::new().expect("tempdir");
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        commit(tmp.path(), "f", "c1");
        git(
            tmp.path(),
            &["branch", "--set-upstream-to", "refs/heads/main", "main"],
        );

        let repo = open(tmp.path());
        let standing = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        )
        .expect("standing");
        assert!(standing.upstream.is_none());
        let target = standing.target.as_ref().expect("target");
        assert_eq!(target.name, "main");
        assert_eq!(target.ahead, Some(0));
        assert_eq!(target.behind, Some(0));
    }

    #[test]
    fn standing_no_counts_skips_walk() {
        let tmp = TempDir::new().expect("tempdir");
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        commit(tmp.path(), "f", "c1");
        git(tmp.path(), &["branch", "topic"]);
        git(tmp.path(), &["checkout", "-q", "topic"]);
        commit(tmp.path(), "f", "c2");

        let repo = open(tmp.path());
        let standing = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: false,
            },
        )
        .expect("standing");
        let target = standing.target.as_ref().expect("target");
        assert_eq!(target.ahead, None);
        assert_eq!(target.behind, None);
        assert!(target.newest_ahead.is_empty());
    }

    #[test]
    fn standing_unborn_and_detached() {
        let tmp = TempDir::new().expect("tempdir");
        git(tmp.path(), &["init", "-q", "-b", "empty"]);
        let repo = open(tmp.path());
        let unborn = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        )
        .expect("standing");
        assert_eq!(unborn.branch.as_deref(), Some("empty"));
        assert!(unborn.target.is_none());

        commit(tmp.path(), "f", "c1");
        let oid = Command::new("git")
            .current_dir(tmp.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let hex = String::from_utf8_lossy(&oid.stdout);
        let hex = hex.trim();
        git(tmp.path(), &["checkout", "--detach", hex]);
        let repo = open(tmp.path());
        let detached = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        )
        .expect("standing");
        assert!(detached.branch.is_none());
        assert!(detached.target.is_none());
        assert!(detached.upstream.is_none());
    }

    #[test]
    fn standing_upstream_counts_match_git() {
        let tmp = TempDir::new().expect("tempdir");
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]);
        commit(&origin, "a", "m1");
        commit(&origin, "a", "m2");
        git(tmp.path(), &["clone", "-q", "origin", "clone"]);
        let clone = tmp.path().join("clone");
        git(&clone, &["config", "user.name", "T"]);
        git(&clone, &["config", "user.email", "t@e.com"]);
        commit(&clone, "b", "local");
        commit(&origin, "a", "remote");
        git(&clone, &["fetch", "-q"]);

        let git_out = Command::new("git")
            .current_dir(&clone)
            .args(["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git rev-list");
        assert!(git_out.status.success());
        let text = String::from_utf8_lossy(&git_out.stdout);
        let mut parts = text.split_whitespace();
        let git_ahead: usize = parts.next().unwrap().parse().unwrap();
        let git_behind: usize = parts.next().unwrap().parse().unwrap();

        let repo = open(&clone);
        let standing = BranchStanding::compute(
            &repo,
            &head_state(&repo),
            StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        )
        .expect("standing");
        let upstream = standing
            .upstream
            .as_ref()
            .or(standing.target.as_ref())
            .expect("upstream or target");
        assert_eq!(upstream.ahead, Some(git_ahead));
        assert_eq!(upstream.behind, Some(git_behind));
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

    #[test]
    fn upstream_ref_for_merge_maps_through_fetch_refspecs() -> TestResult {
        let mut config = ConfigSet::new();
        config
            .add_command_override("remote.origin.fetch", "+refs/heads/*:refs/remotes/mirror/*")?;
        config.add_command_override("remote.origin.fetch", "^refs/heads/wip")?;
        assert_eq!(
            upstream_ref_for_merge(&config, "origin", "refs/heads/main").as_deref(),
            Some("refs/remotes/mirror/main")
        );
        assert_eq!(
            upstream_ref_for_merge(&config, "origin", " main ").as_deref(),
            Some("refs/remotes/mirror/main")
        );
        assert_eq!(upstream_ref_for_merge(&config, "origin", "wip"), None);
        assert_eq!(
            upstream_ref_for_merge(&config, "origin", "refs/tags/v1"),
            None
        );
        assert_eq!(
            upstream_ref_for_merge(&config, "unconfigured", "refs/heads/dev").as_deref(),
            Some("refs/remotes/unconfigured/dev")
        );
        Ok(())
    }

    #[test]
    fn upstream_ref_for_merge_local_remote_is_a_local_ref() {
        let config = ConfigSet::new();
        assert_eq!(
            upstream_ref_for_merge(&config, ".", "main").as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            upstream_ref_for_merge(&config, ".", "refs/heads/dev").as_deref(),
            Some("refs/heads/dev")
        );
    }

    #[test]
    fn upstream_tracking_full_ref_honours_custom_refspec() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "remote.origin.url", "/nonexistent"]);
        git(
            dir,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/remotes/upstream-main",
            ],
        );
        git(dir, &["config", "branch.main.remote", "origin"]);
        git(dir, &["config", "branch.main.merge", "refs/heads/main"]);
        git(dir, &["config", "branch.dev.remote", "origin"]);
        git(dir, &["config", "branch.dev.merge", "refs/heads/dev"]);
        let repo = open(dir);
        assert_eq!(
            upstream_tracking_full_ref(&repo, "main").as_deref(),
            Some("refs/remotes/upstream-main")
        );
        assert_eq!(upstream_tracking_full_ref(&repo, "dev"), None);
        assert_eq!(upstream_tracking_full_ref(&repo, "unset"), None);
    }
}

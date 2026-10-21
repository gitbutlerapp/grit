//! Branch vs remote-tracking comparison for status, checkout, and commit (matches `git/remote.c`).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::error::Result;
use crate::merge_base::{ahead_behind_of_target_commits, count_symmetric_ahead_behind};
use crate::objects::ObjectId;
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::repo::Repository;
use crate::rev_parse::{
    abbreviate_ref_name, resolve_push_full_ref_for_branch, resolve_upstream_symbolic_name,
};
use crate::state::HeadState;
use crate::transfer::{match_positive, ref_excluded};

/// How to compare local HEAD to a remote-tracking ref (`AHEAD_BEHIND_FULL` vs `QUICK`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AheadBehindMode {
    /// Compute exact ahead/behind counts (`rev-list --left-right`).
    Full,
    /// Only detect same vs different (cheap).
    Quick,
}

/// Outcome of comparing `refs/heads/<branch>` to a tracking ref.
#[derive(Clone, Debug)]
pub enum TrackingStat {
    /// Tips are the same commit.
    UpToDate,
    /// Tracking ref is missing (gone upstream).
    Gone {
        /// Short display name for the missing tracking ref.
        display_name: String,
    },
    /// Tips differ; counts are zero in [`AheadBehindMode::Quick`] mode.
    Diverged {
        /// Short display name for the tracking ref.
        display_name: String,
        /// Number of commits local branch is ahead.
        ahead: usize,
        /// Number of commits local branch is behind.
        behind: usize,
    },
}

/// Short display name for a full ref (`refs/remotes/origin/main` -> `origin/main`).
#[must_use]
pub fn shorten_tracking_ref(full_ref: &str) -> String {
    abbreviate_ref_name(full_ref)
}

/// Resolve the local tracking ref for a remote ref using `remote.<name>.fetch`
/// refspecs (Git `remote_find_tracking` / `refspec_find_match` on the fetch list).
///
/// Returns `None` when no positive fetch refspec maps `remote_ref`, or when a
/// negative refspec excludes it. When no fetch refspecs are configured at all,
/// falls back to `refs/remotes/<remote>/<branch>` for `refs/heads/*` only.
#[must_use]
pub fn tracking_ref_for_remote_push_ref(
    git_dir: &Path,
    remote_name: &str,
    remote_ref: &str,
) -> Option<String> {
    if remote_name.contains('/') || remote_name.starts_with('.') {
        return None;
    }

    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .ok()?;
    let (positive, negative) = remote_fetch_refspecs(&config, remote_name);
    if ref_excluded(remote_ref, &negative) {
        return None;
    }
    match match_positive(remote_ref, &positive) {
        Some(Some(dst)) => Some(dst),
        Some(None) => None,
        None if positive.is_empty() => remote_ref
            .strip_prefix("refs/heads/")
            .map(|branch| format!("refs/remotes/{remote_name}/{branch}")),
        None => None,
    }
}

fn remote_fetch_refspecs(
    config: &ConfigSet,
    remote_name: &str,
) -> (Vec<RefspecItem>, Vec<RefspecItem>) {
    let key = format!("remote.{remote_name}.fetch");
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for spec in config.get_all(&key) {
        let Ok(item) = parse_fetch_refspec(&spec) else {
            continue;
        };
        if item.negative {
            negative.push(item);
        } else {
            positive.push(item);
        }
    }
    (positive, negative)
}

/// Update a local remote-tracking ref after a successful push to a named remote.
///
/// The destination ref is resolved through the remote's configured fetch refspecs.
/// Path-like remote names (containing `/` or starting with `.`) skip tracking updates.
///
/// # Parameters
///
/// - `git_dir`: local repository `.git` directory.
/// - `remote_name`: short remote name (e.g. `origin`).
/// - `remote_ref`: destination ref on the remote (e.g. `refs/heads/main`).
/// - `new_oid`: new tip after the push, or `None` when the remote ref was deleted.
///
/// # Errors
///
/// Returns an error if writing or deleting the tracking ref fails.
pub fn update_remote_tracking_ref(
    git_dir: &Path,
    remote_name: &str,
    remote_ref: &str,
    new_oid: Option<ObjectId>,
) -> Result<()> {
    let Some(tracking_ref) = tracking_ref_for_remote_push_ref(git_dir, remote_name, remote_ref)
    else {
        return Ok(());
    };

    match new_oid {
        Some(oid) => {
            if refs::resolve_ref(git_dir, &tracking_ref).ok() == Some(oid) {
                return Ok(());
            }
            refs::write_ref(git_dir, &tracking_ref, &oid)?;
        }
        None => {
            let _ = refs::delete_ref(git_dir, &tracking_ref);
        }
    }
    Ok(())
}

/// Apply remote-tracking ref updates for every successfully pushed ref in `results`.
///
/// Refs reported as [`PushRefStatus::Ok`] or [`PushRefStatus::UpToDate`] are
/// updated (matching Git `transport_update_tracking_ref`). Deletions pass
/// `new_oid: None` to remove the tracking ref.
///
/// # Errors
///
/// Propagates errors from [`refs::commit_ref_store_batch`].
pub fn apply_push_remote_tracking_updates(
    git_dir: &Path,
    remote_name: &str,
    results: &[PushRefResult],
) -> Result<()> {
    let mut batch: Vec<RefUpdate> = Vec::new();
    for result in results {
        if result.status != PushRefStatus::Ok && result.status != PushRefStatus::UpToDate {
            continue;
        }
        let Some(tracking_ref) =
            tracking_ref_for_remote_push_ref(git_dir, remote_name, &result.remote_ref)
        else {
            continue;
        };
        match result.new_oid {
            Some(oid) => {
                if refs::resolve_ref(git_dir, &tracking_ref).ok() == Some(oid) {
                    continue;
                }
                batch.push(RefUpdate {
                    name: tracking_ref,
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Any,
                    reflog: None,
                    flags: RefUpdateFlags::default(),
                });
            }
            None => {
                if refs::resolve_ref(git_dir, &tracking_ref).is_ok() {
                    batch.push(RefUpdate {
                        name: tracking_ref,
                        new_value: None,
                        expected: Expected::Any,
                        reflog: None,
                        flags: RefUpdateFlags::default(),
                    });
                }
            }
        }
    }
    if !batch.is_empty() {
        refs::commit_ref_store_batch(git_dir, &batch)?;
    }
    Ok(())
}

fn branch_head_ref(short_name: &str) -> String {
    format!("refs/heads/{short_name}")
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
        name: shorten_tracking_ref(&full_ref),
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

/// Target and upstream standing for the current HEAD (status header / JSON).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchStanding {
    /// Current branch short name, or `None` when HEAD is detached or invalid.
    pub branch: Option<String>,
    /// Repository target branch comparison, when a target resolves.
    pub target: Option<TrackedComparison>,
    /// Configured upstream comparison, when it resolves and differs from [`Self::target`].
    pub upstream: Option<TrackedComparison>,
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
            HeadState::Invalid | HeadState::Detached { .. } => Ok(Self {
                branch: None,
                target: None,
                upstream: None,
            }),
            HeadState::Branch {
                short_name, oid, ..
            } => {
                let target_base = target_branch(repo)?;
                let upstream_base = upstream_branch(repo, short_name);
                let tip = *oid;
                let target = target_base
                    .map(|base| tracked_comparison(repo, tip, base, opts))
                    .transpose()?;
                let upstream = match upstream_base {
                    Some(base) if target.as_ref().is_some_and(|t| t.full_ref == base.full_ref) => {
                        None
                    }
                    Some(base) => Some(tracked_comparison(repo, tip, base, opts)?),
                    None => None,
                };
                Ok(Self {
                    branch: Some(short_name.clone()),
                    target,
                    upstream,
                })
            }
        }
    }
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

/// Whether `tracking_ref` (a full `refs/remotes/<remote>/<branch>` ref) is the destination of some
/// remote's fetch refspec — i.e. a genuine remote-tracking branch (Git `check_tracking_branch` /
/// `validate_remote_tracking_branch`). A `refs/remotes/...` ref left over after its remote's fetch
/// refspec was narrowed is NOT a valid tracking branch and must not be used for `--track`.
#[must_use]
pub fn remote_tracking_ref_is_mapped(repo: &Repository, tracking_ref: &str) -> bool {
    let Ok(config) = repo.config() else {
        return false;
    };
    for entry in config.entries() {
        // Match keys of the form `remote.<name>.fetch`.
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        if !rest.ends_with(".fetch") {
            continue;
        }
        let Some(spec) = entry.value.as_deref() else {
            continue;
        };
        if refspec_dst_matches(spec, tracking_ref) {
            return true;
        }
    }
    false
}

/// Does the destination half of a fetch refspec (`[+]<src>:<dst>`) match `target`?
/// Supports a single trailing `*` glob, mirroring Git's refspec matching for the common case.
fn refspec_dst_matches(spec: &str, target: &str) -> bool {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let Some((_src, dst)) = spec.split_once(':') else {
        return false;
    };
    let dst = dst.trim();
    if let Some(prefix) = dst.strip_suffix('*') {
        // Glob: dst = `refs/remotes/<remote>/*`; match any ref under that prefix.
        target.starts_with(prefix) && target.len() > prefix.len()
    } else {
        dst == target
    }
}

/// Compare local branch tip to `base_ref` (full ref like `refs/remotes/origin/main`).
pub fn stat_branch_pair(
    repo: &Repository,
    branch_short: &str,
    base_ref: &str,
    mode: AheadBehindMode,
) -> Result<TrackingStat> {
    let branch_ref = branch_head_ref(branch_short);
    let local_oid = match refs::resolve_ref(repo.git_dir(), &branch_ref) {
        Ok(o) => o,
        Err(_) => {
            return Ok(TrackingStat::Diverged {
                display_name: shorten_tracking_ref(base_ref),
                ahead: 0,
                behind: 0,
            });
        }
    };
    let upstream_oid = match refs::resolve_ref(repo.git_dir(), base_ref) {
        Ok(o) => o,
        Err(_) => {
            return Ok(TrackingStat::Gone {
                display_name: shorten_tracking_ref(base_ref),
            });
        }
    };
    if local_oid == upstream_oid {
        return Ok(TrackingStat::UpToDate);
    }
    if mode == AheadBehindMode::Quick {
        return Ok(TrackingStat::Diverged {
            display_name: shorten_tracking_ref(base_ref),
            ahead: 0,
            behind: 0,
        });
    }
    let (ahead, behind) = count_symmetric_ahead_behind(repo, local_oid, upstream_oid)?;
    Ok(TrackingStat::Diverged {
        display_name: shorten_tracking_ref(base_ref),
        ahead,
        behind,
    })
}

/// Read `status.compareBranches` from `.git/config` (`[status]` section or dotted key).
fn parse_status_compare_branches(config_content: &str) -> Option<String> {
    let mut in_status = false;
    for line in config_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_status = trimmed.eq_ignore_ascii_case("[status]");
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("status.comparebranches") {
            return trimmed.split_once('=').map(|(_, v)| v.trim().to_owned());
        }
        if in_status && lower.starts_with("comparebranches") {
            return trimmed.split_once('=').map(|(_, v)| v.trim().to_owned());
        }
    }
    None
}

fn parse_compare_branch_specs(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn resolve_compare_full_ref(repo: &Repository, branch_short: &str, token: &str) -> Option<String> {
    let t = token.trim();
    if t.eq_ignore_ascii_case("@{upstream}") || t.eq_ignore_ascii_case("@{u}") {
        let spec = if branch_short.is_empty() {
            "@{u}".to_string()
        } else {
            format!("{branch_short}@{{u}}")
        };
        resolve_upstream_symbolic_name(repo, &spec).ok()
    } else if t.eq_ignore_ascii_case("@{push}") {
        resolve_push_full_ref_for_branch(repo, branch_short).ok()
    } else {
        None
    }
}

/// Multi-branch tracking lines for porcelain long status and checkout (Git `format_tracking_info`).
pub fn format_tracking_info(
    repo: &Repository,
    branch_short: &str,
    mode: AheadBehindMode,
    show_divergence_advice: bool,
) -> Result<String> {
    let config_path = repo.git_dir().join("config");
    let config_raw = fs::read_to_string(&config_path).unwrap_or_default();
    let compare_raw =
        parse_status_compare_branches(&config_raw).unwrap_or_else(|| "@{upstream}".to_string());

    let tokens = parse_compare_branch_specs(&compare_raw);
    if tokens.is_empty() {
        return Ok(String::new());
    }

    let upstream_full = resolve_compare_full_ref(repo, branch_short, "@{upstream}");
    let push_full = resolve_compare_full_ref(repo, branch_short, "@{push}");

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = String::new();
    let mut reported = false;

    for tok in tokens {
        let Some(full_ref) = resolve_compare_full_ref(repo, branch_short, &tok) else {
            continue;
        };
        if !seen.insert(full_ref.clone()) {
            continue;
        }

        let is_upstream = upstream_full.as_ref() == Some(&full_ref);
        let mut is_push = push_full.as_ref() == Some(&full_ref);
        if is_upstream && push_full.as_ref().is_none_or(|p| p == &full_ref) {
            is_push = true;
        }

        let stat = stat_branch_pair(repo, branch_short, &full_ref, mode)?;

        match &stat {
            TrackingStat::Gone { display_name } if is_upstream => {
                if reported {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "Your branch is based on '{display_name}', but the upstream is gone.\n"
                ));
                out.push_str("  (use \"git branch --unset-upstream\" to fixup)\n");
                reported = true;
            }
            TrackingStat::Gone { .. } => {}
            TrackingStat::UpToDate => {
                if reported {
                    out.push('\n');
                }
                let d = shorten_tracking_ref(&full_ref);
                out.push_str(&format!("Your branch is up to date with '{d}'.\n"));
                reported = true;
            }
            TrackingStat::Diverged {
                display_name,
                ahead,
                behind,
            } => {
                if reported {
                    out.push('\n');
                }
                if mode == AheadBehindMode::Quick {
                    out.push_str(&format!(
                        "Your branch and '{display_name}' refer to different commits.\n"
                    ));
                    if is_push {
                        out.push_str("  (use \"git status --ahead-behind\" for details)\n");
                    }
                } else if *ahead > 0 && *behind > 0 {
                    out.push_str(&format!(
                        "Your branch and '{display_name}' have diverged,\n\
and have {ahead} and {behind} different commits each, respectively.\n"
                    ));
                    if show_divergence_advice && is_upstream {
                        out.push_str(
                            "  (use \"git pull\" if you want to integrate the remote branch with yours)\n",
                        );
                    }
                } else if *ahead > 0 {
                    out.push_str(&format!(
                        "Your branch is ahead of '{display_name}' by {ahead} commit{}.\n",
                        if *ahead == 1 { "" } else { "s" }
                    ));
                    if is_push {
                        out.push_str("  (use \"git push\" to publish your local commits)\n");
                    }
                } else {
                    out.push_str(&format!(
                        "Your branch is behind '{display_name}' by {behind} commit{}, and can be fast-forwarded.\n",
                        if *behind == 1 { "" } else { "s" }
                    ));
                    if is_upstream {
                        out.push_str("  (use \"git pull\" to update your local branch)\n");
                    }
                }
                reported = true;
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        git(tmp.path(), &["clone", "-q", "-b", "main", "origin", "clone"]);
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
}

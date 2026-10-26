//! Stage working-tree changes into the index (`git add`).
//!
//! [`stage`] performs a single index-vs-worktree scan (plus an untracked walk when
//! [`StageMode::All`] is selected). It does **not** compute the index-vs-HEAD diff
//! that full [`super::status::status`] requires.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::diff::{
    empty_blob_oid, is_unregistered_embedded_git_repo, materialize_worktree_blob,
    mode_from_metadata, read_submodule_head_oid, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::hash::Parallelism;
use crate::ignore::IgnoreMatcher;
use crate::index::{
    entry_from_metadata, index_file_mtime, index_path_outcome_key, Index, IndexEntry, IndexLock,
    MODE_GITLINK, MODE_REGULAR, MODE_TREE,
};
use crate::objects::ObjectId;
use crate::odb::BULK_NEW_LOOSE_WRITE;
use crate::path_icase::{
    paths_equal, worktree_dentry_spelling, IcaseIndexCollisionGroups, Stage0IcasePathMap,
};
use crate::path_validate::{validate_path, EntryMode, PathValidationKind, ProtectOptions};
use crate::pathspec::{
    exact_literal_pathspec_set, has_glob_chars, pathspec_is_exclude, sorted_paths_match_pathspec,
    PathspecListMatcher,
};
use crate::porcelain::bulk_index::IndexStagingThresholds;
use crate::porcelain::index_mode::{
    apply_executable_hint_to_index, staged_mode_with_hint, StageExecutableHint,
};
use crate::porcelain::pathspec_match::KnownPathspecChecker;
use crate::porcelain::stage_tracked::{
    replay_deferred_untracked_invalidations, stage_tracked_modifications_in_index_with_rules,
};
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, expand_untracked_for_staging_with_rules, IgnoredMode,
    UntrackedCacheMissAction, UntrackedScan,
};
use crate::progress::ProgressSink;
use crate::repo::Repository;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use crate::worktree_batch::{prepare_and_store_worktree_blobs_parallel, WorktreeBlobReadInput};
use crate::worktree_rules::WorktreeRules;

/// What paths [`stage`] should update (`git add` vs `git add -u`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageMode {
    /// Stage tracked changes and untracked files (default `git add`).
    #[default]
    All,
    /// Stage tracked changes only (`git add -u` / `--update`); untracked paths are ignored.
    Update,
}

/// Inputs for [`stage`].
///
/// Pathspec strings must already be resolved against the work tree (the CLI performs
/// `resolve_pathspec_in_worktree`). When `pathspec_sources` is non-empty it must align
/// with `pathspecs` and supplies the user-facing spelling for errors and exclude magic.
#[derive(Debug, Clone, Default)]
pub struct StageOptions {
    /// Resolved pathspecs used for matching (empty = entire tree).
    pub pathspecs: Vec<String>,
    /// Original pathspec argv fragments; defaults to `pathspecs` when empty.
    pub pathspec_sources: Vec<String>,
    /// Whether to include untracked paths ([`StageMode::All`]) or only tracked changes ([`StageMode::Update`]).
    pub mode: StageMode,
    /// Force executable or non-executable mode for explicitly listed regular files (see [`StageExecutableHint`]).
    pub executable_hint: StageExecutableHint,
    /// Override bulk/parallel staging thresholds (integration tests only).
    #[doc(hidden)]
    pub staging_thresholds: Option<IndexStagingThresholds>,
    /// When true, hash worktree blobs without writing loose objects (commit `--dry-run`); also
    /// classifies paths without persisting the index or UNTR when used from `grit add --dry-run`.
    pub dry_run: bool,
    /// Record intent-to-add entries (empty blob, extended flag) instead of hashing file content.
    pub intent_to_add: bool,
    /// Stage explicitly named ignored paths instead of returning [`Error::PathspecIgnored`].
    pub force_ignored: bool,
    /// Skip [`Error::PathspecIgnored`] validation (caller already checked ignore rules).
    #[doc(hidden)]
    pub skip_pathspec_ignore_check: bool,
}

/// Paths returned by [`stage`] (repository-relative, `/` separators).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageOutcome {
    /// New untracked paths staged at stage 0.
    pub added: Vec<String>,
    /// Tracked paths updated in the index (including conflict resolution and gitlinks).
    pub modified: Vec<String>,
    /// Tracked paths removed from the index because they are absent from the work tree.
    pub removed: Vec<String>,
    /// Paths recorded with the intent-to-add bit (`git add -N`).
    pub intent_to_add: Vec<String>,
}

impl StageOutcome {
    /// Total number of index paths updated.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added
            .len()
            .saturating_add(self.modified.len())
            .saturating_add(self.removed.len())
            .saturating_add(self.intent_to_add.len())
    }
}

fn index_path_to_string(path: &[u8]) -> String {
    index_path_outcome_key(path).into_owned()
}

fn sort_outcome_paths_in_index_order(outcome: &mut StageOutcome, index: &Index) {
    let positions = index.stage0_path_positions();
    let sort_paths = |paths: &mut Vec<String>| {
        paths.sort_by(|a, b| {
            let pos = |p: &str| positions.get(p).copied().unwrap_or(usize::MAX);
            pos(a).cmp(&pos(b)).then_with(|| a.as_str().cmp(b))
        });
        paths.dedup();
    };
    sort_paths(&mut outcome.added);
    sort_paths(&mut outcome.modified);
    sort_paths(&mut outcome.removed);
    sort_paths(&mut outcome.intent_to_add);
}

fn sort_outcome_paths_lexicographic(outcome: &mut StageOutcome) {
    for paths in [
        &mut outcome.added,
        &mut outcome.modified,
        &mut outcome.removed,
        &mut outcome.intent_to_add,
    ] {
        paths.sort();
        paths.dedup();
    }
}

/// Stage every tracked and untracked change for [`crate::porcelain::commit`].
///
/// Uses the stat-trust tracked scan from `stage_tracked_modifications_in_index_with_rules` instead of
/// the diff-for-staging pipeline, then stages untracked paths the same way as [`stage`] with
/// [`StageMode::All`].
///
/// # Errors
///
/// Same as [`stage`].
pub fn stage_for_commit(
    repo: &Repository,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    let opts = StageOptions {
        mode: StageMode::All,
        ..StageOptions::default()
    };
    stage_with_tracked_scan(repo, &opts, progress)
}

/// Like [`stage_for_commit`], but updates only an in-memory index (no `.git/index` write).
///
/// When `dry_run` is true, blob objects are hashed but not written to the object database.
///
/// # Errors
///
/// Same as [`stage_for_commit`].
pub fn stage_for_commit_in_memory(
    repo: &Repository,
    progress: &mut dyn ProgressSink,
    dry_run: bool,
) -> Result<StageOutcome> {
    let mut index = repo.load_index()?;
    stage_for_commit_on_index(repo, &mut index, progress, dry_run)
}

/// Like [`stage_for_commit`], but updates a caller-owned index and does not write it to disk.
///
/// Used by [`crate::porcelain::commit::commit_worktree`] to avoid reloading the index.
pub(crate) fn stage_for_commit_on_index(
    repo: &Repository,
    index: &mut Index,
    progress: &mut dyn ProgressSink,
    dry_run: bool,
) -> Result<StageOutcome> {
    let opts = StageOptions {
        mode: StageMode::All,
        dry_run,
        ..StageOptions::default()
    };
    // Tracked-scan staging writes blobs; use the diff-only path when previewing.
    stage_inner_on_index(repo, &opts, progress, !dry_run, index, false, None, None)
}

/// Stage paths into a caller-owned index without writing it to disk.
///
/// Same matching and validation rules as [`stage`].
pub fn stage_on_index(
    repo: &Repository,
    index: &mut Index,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    stage_on_index_with_pathspec_ignore_exempt(repo, index, opts, progress, None)
}

/// Like [`stage_on_index`], but paths already staged at stage 0 in `ignore_exempt_index` skip
/// [`Error::PathspecIgnored`] (used by path-scoped commits that stage on a parent-tree index).
pub(crate) fn stage_on_index_with_pathspec_ignore_exempt(
    repo: &Repository,
    index: &mut Index,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
    ignore_exempt_index: Option<&Index>,
) -> Result<StageOutcome> {
    stage_inner_on_index(
        repo,
        opts,
        progress,
        false,
        index,
        false,
        None,
        ignore_exempt_index,
    )
}

/// Validate explicit pathspecs against the real index without hashing or staging files.
/// Path-scoped commits use this before constructing their parent-tree index; already
/// indexed paths remain eligible even when an ignore rule now matches them.
pub(crate) fn validate_commit_staging_pathspecs(
    repo: &Repository,
    index: &Index,
    opts: &StageOptions,
) -> Result<()> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let rules = WorktreeRules::from_repository(repo, index)?;
    validate_pathspecs(repo, index, work_tree, &[], &[], opts, &rules, Some(index))
}

/// Stage paths matching `opts` into the index.
///
/// Takes `index.lock`, loads the index while holding it, compares it to the work tree
/// once, hashes only changed or new blobs, removes deleted paths, and invalidates
/// cache-tree / untracked-cache entries for touched paths via [`Index::stage_file`] /
/// [`Index::remove`]. Concurrent callers are serialized by the lock, so an `Ok` result
/// means the staged paths are in the index even when other processes stage at the same
/// time. A dry run reads the index without locking.
///
/// # Errors
///
/// Returns [`Error::LockHeld`] when another process
/// still holds `index.lock` after the `core.filesRefLockTimeout` wait (nothing is staged;
/// the caller may retry), [`Error::Message`] when an explicit pathspec matches nothing,
/// [`Error::PathspecIgnored`] when a path exists but is excluded by ignore rules,
/// or I/O / object-store failures while reading the work tree.
pub fn stage(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    if uses_tracked_scan_fast_path(opts) {
        return stage_with_tracked_scan(repo, opts, progress);
    }
    stage_via_worktree_diff(repo, opts, progress)
}

fn uses_tracked_scan_fast_path(opts: &StageOptions) -> bool {
    if opts.dry_run || opts.intent_to_add {
        return false;
    }
    if !matches!(opts.mode, StageMode::All | StageMode::Update) {
        return false;
    }
    if opts.pathspecs.is_empty() {
        return true;
    }
    if opts.pathspecs.iter().all(|s| s == "." || s == "./") {
        return true;
    }
    pathspecs_allow_tracked_scan(&opts.pathspecs)
}

fn pathspecs_allow_tracked_scan(pathspecs: &[String]) -> bool {
    pathspecs
        .iter()
        .all(|s| !has_glob_chars(s) && !s.starts_with(':') && !pathspec_is_exclude(s))
}

fn normalized_staging_pathspecs(pathspecs: &[String]) -> Cow<'_, [String]> {
    if pathspecs.iter().all(|s| s == "." || s == "./") {
        Cow::Owned(Vec::new())
    } else {
        Cow::Borrowed(pathspecs)
    }
}

fn restricted_index_paths_for_pathspecs(
    index: &Index,
    pathspecs: &[String],
    ctx: crate::pathspec::PathspecMatchContext,
) -> BTreeSet<String> {
    if let Some(exact) = exact_literal_pathspec_set(pathspecs, ctx) {
        if exact.iter().all(|p| {
            index
                .get(p.as_bytes(), 0)
                .is_some_and(|e| e.mode != MODE_TREE)
        }) {
            return exact;
        }
    }
    let matcher = PathspecListMatcher::new(pathspecs, ctx);
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter_map(|e| std::str::from_utf8(&e.path).ok())
        .filter(|p| matcher.matches(p))
        .map(str::to_owned)
        .collect()
}

fn stage_via_worktree_diff(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    stage_inner(repo, opts, progress, false)
}

fn stage_with_tracked_scan(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    stage_inner(repo, opts, progress, true)
}

/// List every untracked file unless the index UNTR cache stores collapsed (normal-mode)
/// directories: matching its `dir_flags` keeps add from rebuilding the cache.
fn untracked_show_all_matching_index(index: &Index) -> bool {
    index
        .untracked_cache
        .as_ref()
        .is_none_or(|uc| uc.dir_flags == 0)
}

fn stage_inner(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
    tracked_scan: bool,
) -> Result<StageOutcome> {
    if opts.dry_run {
        let mut index = repo.load_index()?;
        return stage_inner_on_index(
            repo,
            opts,
            progress,
            tracked_scan,
            &mut index,
            true,
            None,
            None,
        );
    }
    let (mut lock, mut index) = repo.lock_and_load_index()?;
    stage_inner_on_index(
        repo,
        opts,
        progress,
        tracked_scan,
        &mut index,
        true,
        Some(&mut lock),
        None,
    )
}

/// Shared staging engine.
///
/// `on_disk` selects the on-disk `add` semantics (a dry run only lists paths). The index is
/// persisted only through `lock`, which the caller must have taken before loading `index`.
#[allow(clippy::too_many_arguments)]
fn stage_inner_on_index(
    repo: &Repository,
    opts: &StageOptions,
    _progress: &mut dyn ProgressSink,
    tracked_scan: bool,
    index: &mut Index,
    on_disk: bool,
    lock: Option<&mut IndexLock>,
    ignore_exempt_index: Option<&Index>,
) -> Result<StageOutcome> {
    #[cfg(test)]
    repo.odb().hot_path_metrics().record_staging_pass();
    let dry_run = opts.dry_run;
    let dry_run_list_only = dry_run && on_disk;
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;

    let index_path = repo.index_path();
    let index_mtime = index_file_mtime(&index_path);

    // One attribute/ignore/conversion context for the whole add: the worktree diff, the
    // untracked walk and blob conversion all reuse the files it has already loaded.
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(WorktreeRules::from_repository(
        repo, index,
    )?));
    let rules_config = lock_rules(&rules_arc)?.config_arc();
    let parallelism = repo
        .thread_policy()
        .resolve(
            crate::parallel::ParallelOp::IndexRefresh,
            rules_config.as_ref(),
        )
        .parallelism();
    let ignorecase = rules_config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let mut icase_map = Stage0IcasePathMap::from_index(index, ignorecase);

    let staging_pathspecs = normalized_staging_pathspecs(&opts.pathspecs);
    let pathspec_ctx = repo.pathspec_match_context();
    let pathspec_matcher = PathspecListMatcher::new(staging_pathspecs.as_ref(), pathspec_ctx);
    let staging_thresholds = opts.staging_thresholds.unwrap_or_default();
    let use_tracked_scan = tracked_scan && !index.entries.is_empty() && !opts.dry_run;
    let restrict_to_paths = if staging_pathspecs.is_empty() {
        None
    } else {
        Some(Arc::new(restricted_index_paths_for_pathspecs(
            index,
            staging_pathspecs.as_ref(),
            pathspec_ctx,
        )))
    };
    let mut tracked_scan_summary = None;
    let (unstaged, staging_stats) = if index.entries.is_empty() {
        (Vec::new(), std::collections::BTreeMap::new())
    } else if use_tracked_scan {
        let mut rules_guard = lock_rules(&rules_arc)?;
        let summary = stage_tracked_modifications_in_index_with_rules(
            repo,
            work_tree,
            &index_path,
            index,
            &mut rules_guard,
            staging_thresholds,
            restrict_to_paths.as_deref(),
        )?;
        tracked_scan_summary = Some(summary);
        (Vec::new(), std::collections::BTreeMap::new())
    } else {
        let diff_opts = DiffIndexToWorktreeOptions {
            index_mtime,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: Some(std::sync::Arc::clone(&rules_config)),
            worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
            thread_policy: Some(repo.thread_policy()),
            hash_blobs_without_write: opts.dry_run,
            restrict_to_paths: restrict_to_paths.clone(),
            ..DiffIndexToWorktreeOptions::default()
        };
        if dry_run_list_only {
            let (unstaged, _) = crate::diff::diff_index_to_worktree_with_options(
                repo.odb(),
                index,
                work_tree,
                diff_opts,
            )?;
            (unstaged, std::collections::BTreeMap::new())
        } else {
            let (unstaged, _, staging_stats) = crate::diff::diff_index_to_worktree_for_staging(
                repo.odb(),
                repo.git_dir(),
                index,
                work_tree,
                diff_opts,
            )?;
            (unstaged, staging_stats)
        }
    };

    let rules = lock_rules(&rules_arc)?;
    let mut untracked = if opts.mode == StageMode::All {
        let bulk_empty_index_add = index.entries.is_empty() && staging_pathspecs.is_empty();
        // The UNTR cache only answers pathspec-free walks; everywhere else the full listing
        // is one walk, while collapsed `dir/` entries would each need a second walk.
        let show_all = bulk_empty_index_add
            || !staging_pathspecs.is_empty()
            || untracked_show_all_matching_index(index);
        let scan = UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all,
            sort_paths: !bulk_empty_index_add,
            use_untracked_cache: !bulk_empty_index_add,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        };
        let (untracked, _, _) = collect_untracked_and_ignored_inner(
            repo,
            index,
            work_tree,
            staging_pathspecs.as_ref(),
            scan,
            &rules,
            None,
        )?;
        if show_all || untracked.is_empty() {
            untracked
        } else {
            expand_untracked_for_staging_with_rules(
                repo,
                index,
                work_tree,
                untracked,
                staging_pathspecs.as_ref(),
                &rules,
            )?
        }
    } else {
        Vec::new()
    };

    if opts.force_ignored && !opts.pathspecs.is_empty() {
        untracked.extend(collect_force_ignored_untracked(
            repo, index, work_tree, opts, &rules,
        )?);
        untracked.sort_unstable();
        untracked.dedup();
    }

    if !opts.pathspecs.is_empty() {
        validate_pathspecs(
            repo,
            index,
            work_tree,
            &unstaged,
            &untracked,
            opts,
            &rules,
            ignore_exempt_index,
        )?;
    }

    if opts.mode == StageMode::All {
        validate_worktree_paths_for_staging(&rules, &untracked)?;
        for entry in &unstaged {
            if entry.status != DiffStatus::Deleted {
                validate_worktree_paths_for_staging(&rules, &[entry.path().to_owned()])?;
            }
        }
    }

    let matches = |path: &str| staging_pathspecs.is_empty() || pathspec_matcher.matches(path);

    if opts.intent_to_add {
        let mut outcome = stage_intent_to_add_paths(
            repo, work_tree, &untracked, &unstaged, &matches, index, &rules, dry_run,
        )?;
        if dry_run {
            sort_outcome_paths_lexicographic(&mut outcome);
        } else if outcome.total() > 0 {
            sort_outcome_paths_in_index_order(&mut outcome, index);
            index.ensure_canonical_order();
            if let Some(lock) = lock {
                repo.write_index_with_lock(lock, index)?;
            }
        }
        return Ok(outcome);
    }

    let mut outcome = StageOutcome::default();
    let has_unmerged = index.has_unmerged_entries();
    let unmerged_only_paths: HashSet<Vec<u8>> = if has_unmerged {
        index
            .entries
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| e.path.clone())
            .filter(|path| index.get(path, 0).is_none())
            .collect()
    } else {
        HashSet::new()
    };

    if let Some(summary) = &tracked_scan_summary {
        for raw in &summary.modified {
            outcome.modified.push(index_path_to_string(raw));
        }
        for raw in &summary.removed {
            outcome.removed.push(index_path_to_string(raw));
        }
    } else {
        let worktree_updates = collect_tracked_stage_plans(&unstaged, &matches);
        for (path, plan) in worktree_updates {
            if has_unmerged
                && unmerged_only_paths.contains(path.as_bytes())
                && opts.mode != StageMode::Update
            {
                continue;
            }
            if plan.remove {
                if dry_run_list_only {
                    outcome.removed.push(path);
                    continue;
                }
                if index.get(path.as_bytes(), 0).is_some() {
                    index.remove(path.as_bytes());
                    outcome.removed.push(path);
                }
                continue;
            }
            if dry_run_list_only {
                outcome.modified.push(path);
                continue;
            }
            if !dry_run {
                icase_map.remove_alias_of(index, path.as_bytes());
            }
            apply_tracked_stage_plan(
                repo,
                work_tree,
                &path,
                &plan,
                staging_stats.get(&path),
                index,
                has_unmerged,
                opts.executable_hint,
                &matches,
            )?;
            outcome.modified.push(path);
        }
    }

    if ignorecase {
        outcome.modified.extend(stage_ignorecase_spelling_updates(
            work_tree, &icase_map, &matches, index, dry_run,
        )?);
    }

    if !untracked.is_empty() {
        let indexed_any_stage: HashSet<Vec<u8>> =
            index.entries.iter().map(|e| e.path.clone()).collect();
        stage_untracked_paths_parallel(
            repo,
            work_tree,
            &untracked,
            &matches,
            &indexed_any_stage,
            &rules,
            &mut icase_map,
            index,
            parallelism,
            opts.executable_hint,
            dry_run,
            dry_run_list_only,
            &mut outcome,
        )?;
    }

    let mut conflict_marker_rejects = Vec::new();
    if has_unmerged && (!opts.pathspecs.is_empty() || opts.mode == StageMode::Update) {
        let (modified, removed, rejects) = stage_unmerged_paths_matching_pathspecs(
            repo, work_tree, &matches, index, &rules, dry_run,
        )?;
        outcome.modified.extend(modified);
        outcome.removed.extend(removed);
        conflict_marker_rejects.extend(rejects);
    }

    if opts.executable_hint != StageExecutableHint::None && !opts.pathspecs.is_empty() {
        outcome.modified.extend(apply_executable_hint_to_index(
            index,
            opts.executable_hint,
            &matches,
            dry_run,
        )?);
    }

    if dry_run {
        sort_outcome_paths_lexicographic(&mut outcome);
    } else if outcome.total() > 0 {
        sort_outcome_paths_in_index_order(&mut outcome, index);
    }

    if outcome.total() > 0 && !dry_run {
        if let Some(summary) = &tracked_scan_summary {
            replay_deferred_untracked_invalidations(index, summary);
        }
        index.ensure_canonical_order();
        if let Some(lock) = lock {
            repo.write_index_with_lock(lock, index)?;
        }
    }

    if !conflict_marker_rejects.is_empty() {
        conflict_marker_rejects.sort_unstable();
        conflict_marker_rejects.dedup();
        return Err(Error::UnmergedConflictMarkers {
            paths: conflict_marker_rejects,
        });
    }

    Ok(outcome)
}

/// Stage worktree content for explicit pathspecs that match unmerged index entries.
///
/// Index-vs-worktree diffs skip paths with no stage-0 entry, so modify/delete conflicts
/// need an explicit pass when the user runs `grit add <path>`.
fn stage_unmerged_paths_matching_pathspecs(
    repo: &Repository,
    work_tree: &Path,
    matches: &impl Fn(&str) -> bool,
    index: &mut Index,
    rules: &WorktreeRules,
    dry_run: bool,
) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let mut unmerged: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            unmerged.insert(entry.path.clone(), ());
        }
    }
    let mut modified = Vec::new();
    let mut removed = Vec::new();
    let mut rejects = Vec::new();
    for raw_path in unmerged.keys() {
        let rel = match std::str::from_utf8(raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !matches(rel) {
            continue;
        }
        let abs = work_tree.join(rel);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if worktree_path_is_missing(&e) => {
                if dry_run || index.remove(raw_path) {
                    removed.push(rel.to_owned());
                }
                continue;
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let file_attrs = rules.file_attrs(rel, false);
        let marker_size = file_attrs.effective_conflict_marker_size();
        let idx_mode = index
            .entries
            .iter()
            .find(|e| e.path == *raw_path)
            .map(|e| e.mode)
            .unwrap_or(MODE_REGULAR);
        if crate::rerere::worktree_metadata_needs_conflict_marker_inspection(&meta, idx_mode) {
            let wt_bytes = fs::read(&abs).map_err(Error::Io)?;
            if crate::rerere::worktree_bytes_have_unresolved_conflict_markers(
                &wt_bytes,
                marker_size,
            ) {
                rejects.push(rel.to_owned());
                continue;
            }
        }
        if dry_run {
            modified.push(rel.to_owned());
            continue;
        }
        let mode = crate::index::WorktreeModePolicy::from_config(rules.config())
            .index_mode_for_worktree(mode_from_metadata(&meta), Some(idx_mode));
        let oid = materialize_worktree_blob(
            repo.odb(),
            &abs,
            &meta,
            rules.conversion(),
            &file_attrs,
            rel,
            None,
            Some(rules.filter_process()),
        )
        .map_err(|e| Error::Message(format!("could not store {rel}: {e}")))?;
        let mut entry = entry_from_metadata(&meta, raw_path, oid, mode);
        entry.mode = mode;
        index.stage_file(entry);
        index.mark_content_verified(raw_path, oid);
        mark_fsmonitor_staged(index, rel);
        modified.push(rel.to_owned());
    }
    Ok((modified, removed, rejects))
}

fn worktree_path_is_missing(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::NotFound)
}

struct TrackedStagePlan {
    remove: bool,
    oid: ObjectId,
    mode: u32,
    gitlink: bool,
}

fn lock_rules(
    rules: &std::sync::Mutex<WorktreeRules>,
) -> Result<std::sync::MutexGuard<'_, WorktreeRules>> {
    rules
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))
}

/// Merge unstaged diff rows into one staging action per path (unmerged paths may appear twice).
fn collect_tracked_stage_plans(
    unstaged: &[DiffEntry],
    matches: &impl Fn(&str) -> bool,
) -> BTreeMap<String, TrackedStagePlan> {
    let mut changes = BTreeMap::<String, TrackedStagePlan>::new();
    for entry in unstaged {
        let path = entry.path();
        if !matches(path) {
            continue;
        }
        merge_tracked_stage_plan(&mut changes, path.to_owned(), entry);
    }
    changes
}

fn merge_tracked_stage_plan(
    changes: &mut BTreeMap<String, TrackedStagePlan>,
    path: String,
    entry: &DiffEntry,
) {
    match entry.status {
        DiffStatus::Deleted => {
            changes.insert(
                path,
                TrackedStagePlan {
                    remove: true,
                    oid: ObjectId::zero(),
                    mode: 0,
                    gitlink: false,
                },
            );
        }
        DiffStatus::Modified | DiffStatus::TypeChanged | DiffStatus::Added => {
            let mode = mode_from_diff_octal(&entry.new_mode).unwrap_or(0);
            changes.insert(
                path,
                TrackedStagePlan {
                    remove: false,
                    oid: entry.new_oid,
                    mode,
                    gitlink: mode == MODE_GITLINK,
                },
            );
        }
        DiffStatus::Unmerged if entry.new_mode != "000000" => {
            let mode = mode_from_diff_octal(&entry.new_mode).unwrap_or(0);
            changes.entry(path).or_insert(TrackedStagePlan {
                remove: false,
                oid: entry.new_oid,
                mode,
                gitlink: mode == MODE_GITLINK,
            });
        }
        _ => {}
    }
}

fn mode_with_executable_hint(
    base_mode: u32,
    rel_path: &str,
    hint: StageExecutableHint,
    matches: &impl Fn(&str) -> bool,
) -> u32 {
    if hint == StageExecutableHint::None || !matches(rel_path) {
        return base_mode;
    }
    staged_mode_with_hint(base_mode, hint)
}

fn mode_from_diff_octal(mode: &str) -> Result<u32> {
    if mode.is_empty() || mode == "000000" {
        return Ok(0);
    }
    u32::from_str_radix(mode, 8).map_err(|e| Error::Message(format!("invalid mode '{mode}': {e}")))
}

#[allow(clippy::too_many_arguments)]
fn apply_tracked_stage_plan(
    _repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    plan: &TrackedStagePlan,
    cached_stat: Option<&crate::index::IndexStatFields>,
    index: &mut Index,
    has_unmerged: bool,
    executable_hint: StageExecutableHint,
    matches: &impl Fn(&str) -> bool,
) -> Result<()> {
    let abs = work_tree.join(rel_path);

    if plan.gitlink {
        let meta = fs::symlink_metadata(&abs).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("could not read {rel_path}: {e}"),
            ))
        })?;
        let oid = if plan.oid.is_zero() {
            read_submodule_head_oid(&abs).ok_or_else(|| {
                Error::Message(format!("could not resolve submodule HEAD for '{rel_path}'"))
            })?
        } else {
            plan.oid
        };
        return stage_gitlink_at(rel_path, &meta, oid, index);
    }

    let oid = plan.oid;
    let mode = mode_with_executable_hint(plan.mode, rel_path, executable_hint, matches);
    let mut entry = if let Some(stat) = cached_stat {
        stat.index_entry(rel_path.as_bytes(), oid, mode)
    } else {
        let meta = fs::symlink_metadata(&abs).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("could not read {rel_path}: {e}"),
            ))
        })?;
        entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode)
    };
    entry.mode = mode;
    if has_unmerged {
        index.stage_file(entry);
    } else {
        index.add_or_replace(entry);
    }
    mark_fsmonitor_staged(index, rel_path);
    Ok(())
}

/// Refresh index path spellings when the work tree uses a different ASCII case (ignorecase FS).
fn stage_ignorecase_spelling_updates(
    work_tree: &Path,
    icase_map: &Stage0IcasePathMap,
    matches: &impl Fn(&str) -> bool,
    index: &mut Index,
    dry_run: bool,
) -> Result<Vec<String>> {
    let mut updated = Vec::new();
    let stage0: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_GITLINK && e.mode != MODE_TREE)
        .map(|e| e.path.clone())
        .collect();
    let collision_groups = IcaseIndexCollisionGroups::from_index(index, true);

    for raw_path in stage0 {
        let index_rel = match std::str::from_utf8(&raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !matches(index_rel) {
            continue;
        }
        if collision_groups.has_collision_sibling(index_rel) {
            continue;
        }
        let Some(actual_rel) = worktree_dentry_spelling(work_tree, index_rel) else {
            continue;
        };
        if actual_rel == index_rel {
            continue;
        }
        if !paths_equal(raw_path.as_slice(), actual_rel.as_bytes(), true) {
            continue;
        }
        if dry_run {
            updated.push(actual_rel);
            continue;
        }
        let Some(entry) = index
            .entries
            .iter()
            .find(|e| e.path == raw_path && e.stage() == 0)
            .cloned()
        else {
            continue;
        };
        icase_map.remove_alias_of(index, actual_rel.as_bytes());
        let mut refreshed = entry;
        refreshed.path = actual_rel.as_bytes().to_vec();
        index.stage_file(refreshed);
        mark_fsmonitor_staged(index, &actual_rel);
        updated.push(actual_rel);
    }
    Ok(updated)
}

fn embedded_gitlink_root_for_path(
    index: &Index,
    work_tree: &Path,
    path: &str,
    precompose_unicode: bool,
) -> Option<String> {
    let mut rel =
        resolve_worktree_path_for_staging(work_tree, path, precompose_unicode).index_relpath;
    loop {
        if rel.is_empty() {
            return None;
        }
        let abs = work_tree.join(&rel);
        if is_unregistered_embedded_git_repo(index, &rel, &abs) {
            return Some(rel.trim_end_matches('/').to_owned());
        }
        rel = match rel.rfind('/') {
            Some(0) => return None,
            Some(i) => rel[..i].to_string(),
            None => return None,
        };
    }
}

fn collapse_embedded_git_repo_untracked_paths(
    index: &Index,
    work_tree: &Path,
    untracked: &[String],
    precompose_unicode: bool,
) -> Vec<String> {
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let mut plain = Vec::new();
    for path in untracked {
        if let Some(root) =
            embedded_gitlink_root_for_path(index, work_tree, path, precompose_unicode)
        {
            roots.insert(root);
        } else {
            plain.push(path.clone());
        }
    }
    let mut out: Vec<String> = plain
        .into_iter()
        .filter(|path| {
            !roots
                .iter()
                .any(|root| path == root || path.starts_with(&format!("{root}/")))
        })
        .collect();
    out.extend(roots);
    out.sort();
    out.dedup();
    out
}

#[allow(clippy::too_many_arguments)]
fn stage_untracked_paths_parallel(
    repo: &Repository,
    work_tree: &Path,
    untracked: &[String],
    matches: &impl Fn(&str) -> bool,
    indexed_any_stage: &HashSet<Vec<u8>>,
    rules: &WorktreeRules,
    icase_map: &mut Stage0IcasePathMap,
    index: &mut Index,
    parallelism: Parallelism,
    executable_hint: StageExecutableHint,
    dry_run: bool,
    dry_run_list_only: bool,
    outcome: &mut StageOutcome,
) -> Result<()> {
    let precompose_unicode = rules
        .config()
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);

    let untracked =
        collapse_embedded_git_repo_untracked_paths(index, work_tree, untracked, precompose_unicode);

    let mut batch_inputs: Vec<WorktreeBlobReadInput> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();

    for path in &untracked {
        if !matches(path) {
            continue;
        }
        if indexed_any_stage.contains(path.as_bytes()) {
            continue;
        }
        let resolved = resolve_worktree_path_for_staging(work_tree, path, precompose_unicode);
        let meta = fs::symlink_metadata(&resolved.abs).map_err(Error::Io)?;
        if crate::read_dir_kind::metadata_is_git_ignored_special(&meta) {
            continue;
        }
        if is_unregistered_embedded_git_repo(index, &resolved.index_relpath, &resolved.abs) {
            deferred.push(resolved.index_relpath.trim_end_matches('/').to_owned());
            continue;
        }
        let file_attrs = rules.file_attrs(&resolved.index_relpath, false);
        batch_inputs.push(WorktreeBlobReadInput {
            abs: resolved.abs,
            index_relpath: resolved.index_relpath,
            index_entry: None,
            file_attrs,
            cached_meta: Some(meta),
        });
    }

    if !dry_run && !batch_inputs.is_empty() {
        repo.odb().ensure_all_loose_prefix_dirs()?;
    }
    let prepared = if dry_run {
        crate::worktree_batch::prepare_worktree_blobs_parallel(
            repo.odb(),
            &batch_inputs,
            rules.conversion(),
            parallelism,
            Some(rules.filter_process()),
        )?
        .into_iter()
        .map(|prepared| prepared.blob)
        .collect()
    } else {
        prepare_and_store_worktree_blobs_parallel(
            repo.odb(),
            &batch_inputs,
            rules.conversion(),
            parallelism,
            Some(rules.filter_process()),
            BULK_NEW_LOOSE_WRITE,
        )?
    };

    let skip_per_path_cache_invalidate =
        index.cache_tree.is_none() && index.untracked_cache.is_none();
    let mut batch_entries: Vec<IndexEntry> = Vec::with_capacity(prepared.len());
    for prep in prepared {
        let abs = work_tree.join(&prep.index_relpath);
        if prep.meta.is_dir()
            && !prep.meta.file_type().is_symlink()
            && read_submodule_head_oid(&abs).is_some()
        {
            deferred.push(prep.index_relpath);
            continue;
        }
        icase_map.remove_alias_of(index, prep.index_relpath.as_bytes());
        let mode =
            mode_with_executable_hint(prep.mode, &prep.index_relpath, executable_hint, matches);
        let mut entry =
            entry_from_metadata(&prep.meta, prep.index_relpath.as_bytes(), prep.oid, mode);
        entry.mode = mode;
        if !dry_run && !skip_per_path_cache_invalidate {
            index.invalidate_untracked_cache_for_path(&prep.index_relpath);
            index.invalidate_cache_tree_for_path(&entry.path);
        }
        batch_entries.push(entry);
        outcome.added.push(prep.index_relpath);
    }
    let staged_batch_count = batch_entries.len();
    if !dry_run {
        for entry in &batch_entries {
            if let Ok(rel) = std::str::from_utf8(&entry.path) {
                mark_fsmonitor_staged(index, rel);
            }
            index.mark_content_verified(&entry.path, entry.oid);
        }
    }
    if staged_batch_count >= 64 {
        index.replace_stage0_entries_batch(batch_entries);
    } else {
        for entry in batch_entries {
            index.add_or_replace(entry);
        }
    }
    if !dry_run && staged_batch_count > 0 && index.fsmonitor_last_update.is_some() {
        index.fsmonitor_validity_changed();
    }

    for path in deferred {
        if dry_run_list_only {
            outcome.added.push(path);
            continue;
        }
        icase_map.remove_alias_of(index, path.as_bytes());
        stage_untracked_path(
            repo,
            work_tree,
            &path,
            rules,
            index,
            executable_hint,
            dry_run,
            matches,
        )?;
        outcome.added.push(path);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_untracked_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    rules: &WorktreeRules,
    index: &mut Index,
    executable_hint: StageExecutableHint,
    dry_run: bool,
    matches: &impl Fn(&str) -> bool,
) -> Result<()> {
    let rel_path = rel_path.trim_end_matches('/');
    let precompose_unicode = rules
        .config()
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let index_relpath = resolved.index_relpath;
    let meta = fs::symlink_metadata(&abs).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("could not read {rel_path}: {e}"),
        ))
    })?;

    if crate::read_dir_kind::metadata_is_git_ignored_special(&meta) {
        return Ok(());
    }

    if meta.is_dir() && !meta.file_type().is_symlink() && read_submodule_head_oid(&abs).is_some() {
        let head_oid = read_submodule_head_oid(&abs).ok_or_else(|| {
            Error::Message(format!(
                "could not resolve submodule HEAD for '{index_relpath}'"
            ))
        })?;
        return stage_gitlink_at(&index_relpath, &meta, head_oid, index);
    }

    let file_attrs = rules.file_attrs(&index_relpath, false);
    let mode = mode_with_executable_hint(
        mode_from_metadata(&meta),
        &index_relpath,
        executable_hint,
        matches,
    );
    let oid = if dry_run {
        crate::diff::hash_worktree_file(
            repo.odb(),
            &abs,
            &meta,
            rules.conversion(),
            &file_attrs,
            &index_relpath,
            None,
            Some(rules.filter_process()),
        )
        .map_err(|e| Error::Message(format!("could not hash {index_relpath}: {e}")))?
    } else {
        materialize_worktree_blob(
            repo.odb(),
            &abs,
            &meta,
            rules.conversion(),
            &file_attrs,
            &index_relpath,
            None,
            Some(rules.filter_process()),
        )
        .map_err(|e| Error::Message(format!("could not store {index_relpath}: {e}")))?
    };

    let mut entry = entry_from_metadata(&meta, index_relpath.as_bytes(), oid, mode);
    entry.mode = mode;
    index.stage_file(entry);
    index.mark_content_verified(index_relpath.as_bytes(), oid);
    mark_fsmonitor_staged(index, &index_relpath);
    Ok(())
}

fn validate_worktree_paths_for_staging(rules: &WorktreeRules, paths: &[String]) -> Result<()> {
    let protect = ProtectOptions::from_config(rules.config());
    for path in paths {
        if path.ends_with('/') {
            continue;
        }
        validate_path(
            path.as_bytes(),
            EntryMode::new(0o100644),
            protect,
            PathValidationKind::Worktree,
        )
        .map_err(|reason| Error::invalid_path_component(path, reason))?;
    }
    Ok(())
}

/// Stage-0 paths with a conflict still present at stages 1–3, sorted for pathspec matching.
fn unmerged_index_paths(index: &Index) -> Vec<String> {
    if !index.has_unmerged_entries() {
        return Vec::new();
    }
    let mut paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_owned))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

/// Whether `resolved` matches a stage-0 index row (already staged / tracked for commit).
fn pathspec_matches_stage0_index_entries(
    index: &Index,
    resolved: &str,
    ctx: crate::pathspec::PathspecMatchContext,
) -> bool {
    if exact_literal_pathspec_set(&[resolved.to_owned()], ctx).is_some() {
        let path_bytes = resolved.as_bytes();
        if index.get(path_bytes, 0).is_some() {
            return true;
        }
    }
    let stage0_sorted: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_owned))
        .collect();
    if stage0_sorted.is_empty() {
        return false;
    }
    let mut stage0_sorted = stage0_sorted;
    stage0_sorted.sort_unstable();
    stage0_sorted.dedup();
    sorted_paths_match_pathspec(resolved, &stage0_sorted, ctx)
}

fn pathspec_matches_unmerged_index_entries(
    index: &Index,
    unmerged_sorted: &[String],
    resolved: &str,
    ctx: crate::pathspec::PathspecMatchContext,
) -> bool {
    if exact_literal_pathspec_set(&[resolved.to_owned()], ctx).is_some() {
        let path_bytes = resolved.as_bytes();
        if (1..=3).any(|stage| index.get(path_bytes, stage).is_some()) {
            return true;
        }
    }
    if unmerged_sorted.is_empty() {
        return false;
    }
    sorted_paths_match_pathspec(resolved, unmerged_sorted, ctx)
}

#[allow(clippy::too_many_arguments)]
fn validate_pathspecs(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    unstaged: &[DiffEntry],
    untracked: &[String],
    opts: &StageOptions,
    rules: &WorktreeRules,
    ignore_exempt_index: Option<&Index>,
) -> Result<()> {
    let sources = pathspec_sources(opts);
    let positive = sources
        .iter()
        .zip(opts.pathspecs.iter())
        .filter(|(src, _)| !pathspec_is_exclude(src))
        .collect::<Vec<_>>();
    if positive.is_empty() {
        return Ok(());
    }
    let ctx = repo.pathspec_match_context();
    let unmerged_paths = unmerged_index_paths(index);
    let mut checker = KnownPathspecChecker::new(repo);
    let mut ignored_paths = Vec::new();
    let icase_tracked = std::cell::OnceCell::new();
    for (label, resolved) in positive {
        if unstaged.iter().any(|e| {
            let one = [e.path()];
            sorted_paths_match_pathspec(resolved, &one, ctx)
        }) || untracked.iter().any(|p| {
            let one = [p.as_str()];
            sorted_paths_match_pathspec(resolved, &one, ctx)
        }) {
            continue;
        }
        let matches_repo = checker.matches(resolved)?
            || pathspec_matches_unmerged_index_entries(index, &unmerged_paths, resolved, ctx);
        if !matches_repo {
            return Err(Error::Message(format!(
                "pathspec '{label}' did not match any files"
            )));
        }
        if opts.force_ignored || opts.skip_pathspec_ignore_check {
            continue;
        }
        if ignore_exempt_index
            .is_some_and(|exempt| pathspec_matches_stage0_index_entries(exempt, resolved, ctx))
        {
            continue;
        }
        if let Some(display) = ignored_display_for_explicit_add(
            repo,
            index,
            rules,
            work_tree,
            label,
            resolved,
            &icase_tracked,
        )? {
            ignored_paths.push(display);
        }
    }
    if !ignored_paths.is_empty() {
        ignored_paths.sort_unstable();
        ignored_paths.dedup();
        return Err(Error::PathspecIgnored {
            paths: ignored_paths,
        });
    }
    Ok(())
}

fn ignored_display_for_explicit_add<'i>(
    repo: &Repository,
    index: &'i Index,
    rules: &WorktreeRules,
    work_tree: &Path,
    label: &str,
    resolved: &str,
    icase_tracked: &std::cell::OnceCell<crate::path_icase::Stage0TrackedPaths<'i>>,
) -> Result<Option<String>> {
    if pathspec_is_exclude(resolved)
        || has_glob_chars(resolved)
        || resolved.starts_with(":(")
        || resolved.starts_with(':')
    {
        return Ok(None);
    }
    let ctx = repo.pathspec_match_context();
    let precompose = repo.pathspec_precompose_enabled();
    let staged = resolve_worktree_path_for_staging(work_tree, resolved, precompose);
    if !staged.abs.exists() {
        return Ok(None);
    }
    let rel = staged.index_relpath;
    let is_dir = staged.abs.is_dir();
    let mut ignore = rules.ignore_mut();
    if is_dir {
        if pathspec_matches_stage0_index_entries(index, resolved, ctx) {
            return Ok(None);
        }
        let (ignored, _) = ignore.check_path(repo, Some(index), &rel, true)?;
        return Ok(ignored.then(|| label.to_owned()));
    }
    let (ignored, _) = ignore.check_path(repo, Some(index), &rel, false)?;
    if !ignored {
        return Ok(None);
    }
    if let Some(parent) = first_ignored_parent_directory(repo, index, &mut ignore, &rel)? {
        return Ok(Some(parent));
    }
    // Under `core.ignorecase` a file whose index entry differs only in case is tracked, and
    // Git stages it rather than reporting it as ignored.
    if ignore.ignore_case()
        && icase_tracked
            .get_or_init(|| {
                crate::path_icase::Stage0TrackedPaths::from_index_for_untracked_scan(index, true)
            })
            .contains_bytes(rel.as_bytes())
    {
        return Ok(None);
    }
    Ok(Some(label.to_owned()))
}

fn first_ignored_parent_directory(
    repo: &Repository,
    index: &Index,
    ignore: &mut IgnoreMatcher,
    rel: &str,
) -> Result<Option<String>> {
    let mut parts: Vec<&str> = rel.split('/').collect();
    if parts.len() <= 1 {
        return Ok(None);
    }
    while parts.len() > 1 {
        parts.pop();
        let prefix = parts.join("/");
        let (ignored, _) = ignore.check_path(repo, Some(index), &prefix, true)?;
        if ignored {
            return Ok(Some(prefix));
        }
    }
    Ok(None)
}

fn collect_force_ignored_untracked(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    opts: &StageOptions,
    rules: &WorktreeRules,
) -> Result<Vec<String>> {
    let sources = pathspec_sources(opts);
    let mut out = Vec::new();
    let precompose = repo.pathspec_precompose_enabled();
    let mut ignore = rules.ignore_mut();
    for (label, resolved) in sources.iter().zip(opts.pathspecs.iter()) {
        if pathspec_is_exclude(label) {
            continue;
        }
        if has_glob_chars(resolved) || resolved.starts_with(":(") || resolved.starts_with(':') {
            continue;
        }
        let staged = resolve_worktree_path_for_staging(work_tree, resolved, precompose);
        if !staged.abs.exists() {
            continue;
        }
        let rel = staged.index_relpath;
        let meta = fs::symlink_metadata(&staged.abs).map_err(Error::Io)?;
        let is_dir = meta.is_dir() && !meta.file_type().is_symlink();
        let (ignored, _) = ignore.check_path(repo, Some(index), &rel, is_dir)?;
        if !ignored {
            continue;
        }
        if is_dir {
            walk_worktree_files_under(work_tree, &rel, &mut out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(out)
}

fn walk_worktree_files_under(work_tree: &Path, rel_dir: &str, out: &mut Vec<String>) -> Result<()> {
    let abs_dir = work_tree.join(rel_dir);
    let mut stack = vec![abs_dir];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(Error::Io)? {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            let meta = entry.metadata().map_err(Error::Io)?;
            if meta.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !meta.is_file() && !meta.file_type().is_symlink() {
                continue;
            }
            let rel = pathdiff_worktree(work_tree, &path)?;
            out.push(rel);
        }
    }
    Ok(())
}

fn pathdiff_worktree(work_tree: &Path, abs: &Path) -> Result<String> {
    let rel = abs
        .strip_prefix(work_tree)
        .map_err(|_| Error::Message("path escaped the work tree during force-add walk".into()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

#[allow(clippy::too_many_arguments)]
fn stage_intent_to_add_paths(
    repo: &Repository,
    work_tree: &Path,
    untracked: &[String],
    unstaged: &[DiffEntry],
    matches: &impl Fn(&str) -> bool,
    index: &mut Index,
    rules: &WorktreeRules,
    dry_run: bool,
) -> Result<StageOutcome> {
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for path in untracked {
        if matches(path) {
            candidates.insert(path.clone());
        }
    }
    for entry in unstaged {
        let path = entry.path().to_owned();
        if !matches(&path) {
            continue;
        }
        if index.get(path.as_bytes(), 0).is_some() {
            continue;
        }
        if entry.status == DiffStatus::Deleted {
            continue;
        }
        candidates.insert(path);
    }

    let mut outcome = StageOutcome::default();
    let empty = empty_blob_oid();
    if !dry_run {
        repo.odb()
            .write(crate::objects::ObjectKind::Blob, b"")
            .map_err(|e| Error::Message(format!("could not store empty blob: {e}")))?;
    }
    let precompose = rules
        .config()
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);

    let mut file_paths: BTreeSet<String> = BTreeSet::new();
    for rel in candidates {
        let resolved = resolve_worktree_path_for_staging(work_tree, &rel, precompose);
        let meta = fs::symlink_metadata(&resolved.abs).map_err(Error::Io)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            let mut under = Vec::new();
            walk_worktree_files_under(work_tree, &resolved.index_relpath, &mut under)?;
            file_paths.extend(under);
        } else if meta.is_file() || meta.file_type().is_symlink() {
            file_paths.insert(resolved.index_relpath);
        }
    }

    for rel in file_paths {
        if index.get(rel.as_bytes(), 0).is_some() {
            continue;
        }
        let resolved = resolve_worktree_path_for_staging(work_tree, &rel, precompose);
        let meta = fs::symlink_metadata(&resolved.abs).map_err(Error::Io)?;
        if dry_run {
            outcome.intent_to_add.push(rel);
            continue;
        }
        let mode = mode_from_metadata(&meta);
        let mut entry = entry_from_metadata(&meta, resolved.index_relpath.as_bytes(), empty, mode);
        entry.oid = empty;
        entry.size = 0;
        entry.set_intent_to_add(true);
        index.stage_file(entry);
        mark_fsmonitor_staged(index, &resolved.index_relpath);
        outcome.intent_to_add.push(resolved.index_relpath);
    }
    Ok(outcome)
}

fn pathspec_sources(opts: &StageOptions) -> Vec<String> {
    if opts.pathspec_sources.is_empty() {
        opts.pathspecs.clone()
    } else {
        opts.pathspec_sources.clone()
    }
}

fn stage_gitlink_at(
    rel_path: &str,
    meta: &fs::Metadata,
    head_oid: ObjectId,
    index: &mut Index,
) -> Result<()> {
    let mut entry = entry_from_metadata(meta, rel_path.as_bytes(), head_oid, MODE_GITLINK);
    entry.size = 0;
    index.stage_file(entry);
    mark_fsmonitor_staged(index, rel_path);
    Ok(())
}

fn mark_fsmonitor_staged(index: &mut Index, rel_path: &str) {
    if index.fsmonitor_last_update.is_some() {
        if let Some(staged) = index.get_mut(rel_path.as_bytes(), 0) {
            staged.set_fsmonitor_valid(true);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use crate::environment::RepositoryOptions;

    use super::*;
    use crate::index::{entry_from_stat, IndexEntry, MODE_GITLINK, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::progress::NullProgress;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_repo(root: &Path) -> Repository {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        Repository::open(&git, &RepositoryOptions::empty().with_work_tree(root)).unwrap()
    }

    fn stage_file_in_index(repo: &Repository, index: &mut Index, rel: &str, contents: &[u8]) {
        let wt = repo.work_tree().unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, contents).unwrap();
        let oid = repo.odb().write(ObjectKind::Blob, contents).unwrap();
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }

    fn write_index(repo: &Repository, index: &mut Index) {
        index.sort();
        repo.write_index(index).unwrap();
    }

    #[test]
    fn commit_pathspec_validation_does_not_hash_or_stage_contents() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "large.bin", &vec![b'x'; 2 * 1024 * 1024]);
        write_index(&repo, &mut index);
        fs::write(
            dir.path().join("large.bin"),
            vec![b'y'; 2 * 1024 * 1024 + 1],
        )
        .unwrap();
        fs::write(dir.path().join("new.bin"), b"ignored").unwrap();
        fs::write(dir.path().join(".gitignore"), "*.bin\n").unwrap();
        let opts = StageOptions {
            pathspecs: vec!["large.bin".into()],
            ..StageOptions::default()
        };
        let hashes = repo.odb().hot_path_metrics().stream_hash_calls();
        let writes = repo.odb().hot_path_metrics().stream_write_calls();
        validate_commit_staging_pathspecs(&repo, &index, &opts).unwrap();
        assert_eq!(repo.odb().hot_path_metrics().stream_hash_calls(), hashes);
        assert_eq!(repo.odb().hot_path_metrics().stream_write_calls(), writes);
        let opts = StageOptions {
            pathspecs: vec!["new.bin".into()],
            ..opts
        };
        assert!(matches!(
            validate_commit_staging_pathspecs(&repo, &index, &opts),
            Err(Error::PathspecIgnored { .. })
        ));
    }

    #[test]
    fn sort_outcome_paths_in_index_order_non_utf8_display_key() {
        use crate::objects::ObjectId;

        let empty = ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let bad_path = b"bad\xffname.txt".to_vec();
        let mut index = Index::new();
        for path in [b"a.txt".as_slice(), bad_path.as_slice()] {
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_REGULAR,
                uid: 0,
                gid: 0,
                size: 1,
                oid: empty,
                flags: path.len().min(0xfff) as u16,
                flags_extended: None,
                path: path.to_vec(),
                base_index_pos: 0,
            });
        }
        index.sort();

        let mut outcome = StageOutcome::default();
        outcome
            .modified
            .push(index_path_to_string(bad_path.as_slice()));
        outcome.modified.push(index_path_to_string(b"a.txt"));

        sort_outcome_paths_in_index_order(&mut outcome, &index);

        let expected: Vec<String> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| index_path_to_string(&e.path))
            .collect();
        assert_eq!(outcome.modified, expected);
    }

    fn cache_tree_node_valid(index: &Index, path: &str) -> Option<bool> {
        let root = index.cache_tree.as_ref()?;
        let mut current = root;
        if !path.is_empty() {
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                current = current
                    .children
                    .iter()
                    .find(|c| c.name == component.as_bytes())?;
            }
        }
        Some(current.is_valid())
    }

    #[test]
    fn sort_outcome_paths_follows_index_entry_order() {
        let null = empty_blob_oid();
        let mut index = Index::new();
        for i in 0..1000 {
            let path = format!(
                "mod{:02}/pkg{:02}/sub{}/src/file{:03}.c",
                i % 20,
                (i / 20) % 20,
                (i / 400) % 10,
                i % 50
            );
            index.add_or_replace(conflict_index_entry(&path, 0, null, MODE_REGULAR));
        }
        index.sort();

        let mut added: Vec<String> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| index_path_to_string(&e.path))
            .collect();
        added.reverse();

        let mut outcome = StageOutcome {
            added,
            ..StageOutcome::default()
        };
        sort_outcome_paths_in_index_order(&mut outcome, &index);

        let expected: Vec<String> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| index_path_to_string(&e.path))
            .collect();
        assert_eq!(outcome.added, expected);
    }

    #[test]
    fn precomposed_repo_stages_many_literal_paths_and_nfd_spellings() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        let mut tracked = Vec::new();
        for i in 0..2_000 {
            let rel = format!("d{}/e{}/f{}/file{i}.txt", i % 7, (i / 7) % 7, (i / 49) % 7);
            stage_file_in_index(&repo, &mut index, &rel, format!("{i}\n").as_bytes());
            tracked.push(rel);
        }
        stage_file_in_index(&repo, &mut index, "caf\u{e9}/menu.txt", b"old\n");
        write_index(&repo, &mut index);

        let mut changed: Vec<String> = tracked.iter().step_by(40).cloned().collect();
        for rel in &changed {
            fs::write(root.join(rel), b"changed\n").unwrap();
        }
        fs::write(root.join("caf\u{e9}/menu.txt"), b"new\n").unwrap();

        let fresh = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        fresh.force_pathspec_precompose_for_test();
        let mut pathspecs = changed.clone();
        pathspecs.push("cafe\u{301}/menu.txt".to_owned());
        pathspecs.push(tracked[1].clone());
        let opts = StageOptions {
            pathspecs,
            ..StageOptions::default()
        };
        let outcome = stage(&fresh, &opts, &mut NullProgress).unwrap();

        changed.push("caf\u{e9}/menu.txt".to_owned());
        changed.sort();
        let mut modified = outcome.modified.clone();
        modified.sort();
        assert_eq!(modified, changed);
        assert!(outcome.added.is_empty() && outcome.removed.is_empty());
        let index = fresh.load_index().unwrap();
        let changed_oid = fresh.odb().write(ObjectKind::Blob, b"changed\n").unwrap();
        for rel in changed.iter().filter(|p| p.starts_with('d')) {
            assert_eq!(index.get(rel.as_bytes(), 0).unwrap().oid, changed_oid);
        }
        assert_eq!(index.entries.len(), tracked.len() + 1);
    }

    #[test]
    fn bulk_empty_index_stage_writes_sorted_index() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/x.txt"), b"2\n").unwrap();
        fs::write(root.join("z.txt"), b"3\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.added.len(), 3);

        let index = repo.load_index().unwrap();
        let paths: Vec<String> = index
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        assert_eq!(paths, ["a.txt", "d/x.txt", "z.txt"]);
    }

    #[test]
    fn stage_outcome_sort_matches_index_order_many_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        for a in 0..8_u8 {
            for b in 0..8 {
                let dir = root.join(format!("big/{a}/{b}"));
                fs::create_dir_all(&dir).unwrap();
                for f in 0..12_u8 {
                    fs::write(dir.join(format!("f{f}.txt")), b"x\n").unwrap();
                }
            }
        }
        let expected_count = 8 * 8 * 12;
        let outcome = stage(
            &repo,
            &StageOptions {
                staging_thresholds: Some(IndexStagingThresholds {
                    large_index_restage: 0,
                    parallel_tracked_restage: 0,
                }),
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.added.len(), expected_count);

        let index = repo.load_index().unwrap();
        let index_order: Vec<String> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        assert_eq!(outcome.added, index_order);
    }

    #[test]
    fn stage_all_adds_modifies_deletes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "keep.txt", b"keep\n");
        stage_file_in_index(&repo, &mut index, "modify.txt", b"old\n");
        stage_file_in_index(&repo, &mut index, "remove.txt", b"gone\n");
        write_index(&repo, &mut index);

        fs::write(root.join("modify.txt"), b"new\n").unwrap();
        fs::remove_file(root.join("remove.txt")).unwrap();
        fs::write(root.join("fresh.txt"), b"added\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.added.len(), 1);
        assert_eq!(outcome.modified.len(), 1);
        assert_eq!(outcome.removed.len(), 1);

        let index = repo.load_index().unwrap();
        assert!(index.get(b"fresh.txt", 0).is_some());
        assert!(index.get(b"modify.txt", 0).is_some());
        assert!(index.get(b"remove.txt", 0).is_none());
    }

    #[test]
    fn stage_adds_untracked_when_index_nonempty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"tracked\n");
        write_index(&repo, &mut index);
        fs::write(root.join("new.txt"), b"fresh\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.added.len(), 1);
        let index = repo.load_index().unwrap();
        assert!(index.get(b"new.txt", 0).is_some());
    }

    #[test]
    fn stage_for_commit_adds_untracked_when_index_nonempty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"tracked\n");
        write_index(&repo, &mut index);
        fs::write(root.join("new.txt"), b"fresh\n").unwrap();

        let outcome = stage_for_commit(&repo, &mut NullProgress).unwrap();
        assert_eq!(outcome.added.len(), 1);
        let index = repo.load_index().unwrap();
        assert!(index.get(b"new.txt", 0).is_some());
    }

    #[test]
    fn stage_pathspec_limits_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "a.txt", b"a\n");
        stage_file_in_index(&repo, &mut index, "b.txt", b"b\n");
        write_index(&repo, &mut index);
        fs::write(root.join("a.txt"), b"a2\n").unwrap();
        fs::write(root.join("b.txt"), b"b2\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["a.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified.len(), 1);
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        let a_oid = index.get(b"a.txt", 0).unwrap().oid;
        let b_oid = index.get(b"b.txt", 0).unwrap().oid;
        let a_blob = repo.odb().read(&a_oid).unwrap();
        let b_blob = repo.odb().read(&b_oid).unwrap();
        assert_eq!(a_blob.data, b"a2\n");
        assert_eq!(b_blob.data, b"b\n");
    }

    #[test]
    fn stage_untracked_pathspec_does_not_stage_unrelated_tracked_modifications() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"tracked\n");
        write_index(&repo, &mut index);
        fs::write(root.join("tracked.txt"), b"tracked-modified\n").unwrap();
        fs::write(root.join("new.txt"), b"fresh\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["new.txt".to_owned()],
                pathspec_sources: vec!["new.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.added, vec!["new.txt".to_owned()]);
        assert!(
            outcome.modified.is_empty(),
            "must not stage unrelated tracked edits"
        );

        let index = repo.load_index().unwrap();
        let tracked_oid = index.get(b"tracked.txt", 0).unwrap().oid;
        let tracked_blob = repo.odb().read(&tracked_oid).unwrap();
        assert_eq!(
            tracked_blob.data, b"tracked\n",
            "tracked.txt index entry must stay at pre-add content"
        );
    }

    #[test]
    fn stage_validates_many_pathspecs_against_index_and_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        for rel in [
            "dir/a.txt",
            "dir/b.txt",
            "dir-x.txt",
            "dirx/c.txt",
            "top.txt",
        ] {
            stage_file_in_index(&repo, &mut index, rel, b"1\n");
        }
        write_index(&repo, &mut index);
        for rel in ["dir/a.txt", "dir/b.txt", "dirx/c.txt", "top.txt"] {
            fs::write(root.join(rel), b"2\n").unwrap();
        }
        fs::write(root.join("new.txt"), b"n\n").unwrap();

        let specs = ["dir/", "dirx", "top.txt", "new.txt"].map(str::to_owned);
        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: specs.to_vec(),
                pathspec_sources: specs.to_vec(),
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified.len(), 4);
        assert_eq!(outcome.added.len(), 1);
        let index = repo.load_index().unwrap();
        assert!(index.get(b"new.txt", 0).is_some());

        for missing in ["di", "dir/a", "top.txt/", "nope"] {
            let err = stage(
                &repo,
                &StageOptions {
                    pathspecs: vec!["top.txt".to_owned(), missing.to_owned()],
                    pathspec_sources: vec!["top.txt".to_owned(), missing.to_owned()],
                    ..StageOptions::default()
                },
                &mut NullProgress,
            )
            .unwrap_err();
            assert!(
                matches!(&err, Error::Message(msg) if msg.contains(&format!("'{missing}'"))),
                "{missing}: {err:?}"
            );
        }
    }

    #[test]
    fn stage_errors_on_explicit_ignored_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("a.log"), b"x\n").unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["a.log".to_owned()],
                pathspec_sources: vec!["a.log".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(matches!(err, Error::PathspecIgnored { .. }));
    }

    #[test]
    fn stage_errors_on_explicit_ignored_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), b"y\n").unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["build".to_owned()],
                pathspec_sources: vec!["build".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        match err {
            Error::PathspecIgnored { paths } => assert_eq!(paths, vec!["build".to_owned()]),
            other => panic!("expected PathspecIgnored, got {other:?}"),
        }
    }

    #[test]
    fn stage_skips_ignored_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/x.txt"), b"nope\n").unwrap();
        fs::write(root.join("tracked-new.txt"), b"yes\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked-new.txt", 0).is_some());
        assert!(index.get(b"ignored/x.txt", 0).is_none());
    }

    #[test]
    fn stage_precomposes_untracked_index_path_while_reading_nfd_disk_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tprecomposeunicode = true\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        let nfd = "cafe\u{301}.txt";
        fs::write(root.join(nfd), b"content\n").unwrap();

        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        assert_eq!(outcome.added.len(), 1);
        let index = repo.load_index().unwrap();
        let entry = index.get("caf\u{e9}.txt".as_bytes(), 0).unwrap();
        assert!(index.get(nfd.as_bytes(), 0).is_none());
        assert_eq!(repo.odb().read(&entry.oid).unwrap().data, b"content\n");
    }

    #[test]
    fn stage_skips_clean_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "clean.txt", b"same\n");
        stage_file_in_index(&repo, &mut index, "dirty.txt", b"old\n");
        write_index(&repo, &mut index);
        fs::write(root.join("dirty.txt"), b"new\n").unwrap();

        repo.odb().enable_mem_overlay();
        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.modified.len(), 1);
        assert_eq!(outcome.total(), 1);

        let overlay_len = repo
            .odb()
            .mem_overlay_len_for_tests()
            .expect("overlay enabled");
        assert_eq!(
            overlay_len, 1,
            "only the dirty blob should be hashed/written"
        );
    }

    #[test]
    fn stage_invalidates_cache_tree_paths_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "alpha/one.txt", b"1\n");
        stage_file_in_index(&repo, &mut index, "beta/two.txt", b"2\n");
        let cache = crate::write_tree::build_cache_tree_from_index(&repo.odb(), &index).unwrap();
        index.set_cache_tree(cache);
        write_index(&repo, &mut index);

        fs::write(root.join("beta/two.txt"), b"22\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let index = repo.load_index().unwrap();
        assert_eq!(cache_tree_node_valid(&index, ""), Some(false));
        assert_eq!(cache_tree_node_valid(&index, "alpha"), Some(true));
        assert_eq!(cache_tree_node_valid(&index, "beta"), Some(false));
    }

    fn conflict_index_entry(path: &str, stage: u8, oid: ObjectId, mode: u32) -> IndexEntry {
        let path_bytes = path.as_bytes().to_vec();
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: (path_bytes.len().min(0xFFF) as u16) | ((stage as u16) << 12),
            flags_extended: None,
            path: path_bytes,
            base_index_pos: 0,
        }
    }

    fn conflict_marker_lines(size: usize) -> (String, String, String) {
        (
            format!("{} HEAD", "<".repeat(size)),
            "=".repeat(size),
            format!("{} side", ">".repeat(size)),
        )
    }

    fn write_marked_conflict_with_marker_size(root: &Path, rel: &str, size: usize) {
        let (open, mid, close) = conflict_marker_lines(size);
        fs::write(
            root.join(rel),
            format!("base\n{open}\nours\n{mid}\ntheirs\n{close}\n"),
        )
        .unwrap();
    }

    fn write_three_stage_conflict_index(
        repo: &Repository,
        index: &mut Index,
        path: &str,
        base: &[u8],
        ours: &[u8],
        theirs: &[u8],
    ) {
        let base_oid = repo.odb().write(ObjectKind::Blob, base).unwrap();
        let ours_oid = repo.odb().write(ObjectKind::Blob, ours).unwrap();
        let theirs_oid = repo.odb().write(ObjectKind::Blob, theirs).unwrap();
        index
            .entries
            .push(conflict_index_entry(path, 1, base_oid, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry(path, 2, ours_oid, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry(path, 3, theirs_oid, MODE_REGULAR));
    }

    #[test]
    fn explicit_add_rejects_unresolved_conflict_markers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        write_three_stage_conflict_index(&repo, &mut index, "f", b"base\n", b"ours\n", b"theirs\n");
        write_index(&repo, &mut index);
        fs::write(
            root.join("f"),
            b"base\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> side\n",
        )
        .unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::UnmergedConflictMarkers { .. }),
            "explicit add must refuse marked file: {err}"
        );
        if let Error::UnmergedConflictMarkers { paths } = err {
            assert_eq!(paths, vec!["f".to_owned()]);
        }

        let index = repo.load_index().unwrap();
        assert!(
            index.has_unmerged_entries(),
            "refusal must leave conflict stages intact"
        );
        assert!(index.get(b"f", 0).is_none());
    }

    #[test]
    fn validate_pathspec_matches_unmerged_index_without_stage_zero() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        write_three_stage_conflict_index(&repo, &mut index, "f", b"base\n", b"ours\n", b"theirs\n");
        write_index(&repo, &mut index);

        let result = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        );
        assert!(
            !matches!(
                &result,
                Err(Error::Message(msg)) if msg.contains("did not match any files")
            ),
            "pathspec validation must see unmerged index stages: {result:?}"
        );
    }

    #[test]
    fn explicit_add_dry_run_rejects_unresolved_conflict_markers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        write_three_stage_conflict_index(&repo, &mut index, "f", b"base\n", b"ours\n", b"theirs\n");
        write_index(&repo, &mut index);
        fs::write(
            root.join("f"),
            b"base\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> side\n",
        )
        .unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::UnmergedConflictMarkers { .. }),
            "dry-run explicit add must not claim success: {err}"
        );
    }

    #[test]
    fn explicit_add_rejects_custom_conflict_marker_size() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".gitattributes"), "f conflict-marker-size=10\n").unwrap();
        let repo = init_repo(root);
        let mut index = Index::new();
        write_three_stage_conflict_index(&repo, &mut index, "f", b"base\n", b"ours\n", b"theirs\n");
        write_index(&repo, &mut index);
        write_marked_conflict_with_marker_size(root, "f", 10);

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::UnmergedConflictMarkers { .. }),
            "must honor conflict-marker-size=10: {err}"
        );
        assert!(repo.load_index().unwrap().has_unmerged_entries());
    }

    #[test]
    fn explicit_add_dry_run_rejects_custom_conflict_marker_size() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".gitattributes"), "f conflict-marker-size=10\n").unwrap();
        let repo = init_repo(root);
        let mut index = Index::new();
        write_three_stage_conflict_index(&repo, &mut index, "f", b"base\n", b"ours\n", b"theirs\n");
        write_index(&repo, &mut index);
        write_marked_conflict_with_marker_size(root, "f", 10);

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::UnmergedConflictMarkers { .. }),
            "{err}"
        );
    }

    #[test]
    fn explicit_add_modify_delete_rejects_unresolved_conflict_markers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"base\nfeat\n").unwrap();
        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);
        fs::write(
            root.join("f"),
            b"base\n<<<<<<< HEAD\n=======\nfeat\n>>>>>>> side\n",
        )
        .unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::UnmergedConflictMarkers { .. }),
            "{err}"
        );
        let index = repo.load_index().unwrap();
        assert!(index.has_unmerged_entries());
    }

    #[test]
    fn broad_add_stages_resolved_unmerged_paths_and_rejects_marked_siblings() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let c_oid = repo.odb().write(ObjectKind::Blob, b"new\n").unwrap();
        let mut index = Index::new();
        write_three_stage_conflict_index(
            &repo,
            &mut index,
            "a.txt",
            b"1\n2\n3\n",
            b"1\n2\n3\nmain\n",
            b"1\n2\n3\ntopic\n",
        );
        write_three_stage_conflict_index(
            &repo,
            &mut index,
            "b.txt",
            b"1\n2\n3\n",
            b"1\n2\n3\nmain\n",
            b"1\n2\n3\ntopic\n",
        );
        index
            .entries
            .push(conflict_index_entry("c.txt", 0, c_oid, MODE_REGULAR));
        write_index(&repo, &mut index);
        fs::write(root.join("a.txt"), b"1\n2\n3\nok\n").unwrap();
        fs::write(
            root.join("b.txt"),
            b"1\n2\n3\n<<<<<<< HEAD\nmain\n=======\ntopic\n>>>>>>> side\n",
        )
        .unwrap();
        fs::write(root.join("c.txt"), b"changed\n").unwrap();

        let err = stage(
            &repo,
            &StageOptions {
                pathspecs: vec![".".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap_err();
        let Error::UnmergedConflictMarkers { paths } = err else {
            panic!("expected marker rejection, got {err}");
        };
        assert_eq!(paths, vec!["b.txt".to_owned()]);

        let index = repo.load_index().unwrap();
        assert!(
            index.get(b"a.txt", 0).is_some(),
            "resolved a.txt must be staged"
        );
        assert!(
            index.get(b"c.txt", 0).is_some(),
            "unrelated c.txt must be staged"
        );
        assert!(index.get(b"b.txt", 0).is_none(), "b.txt must stay unmerged");
        assert!(index.has_unmerged_entries());
    }

    #[test]
    fn stage_resolved_modify_delete_conflict_without_markers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"base\nfeat\n").unwrap();

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);

        fs::write(root.join("f"), b"base\nfeat\n").unwrap();
        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified.len(), 1);

        let index = repo.load_index().unwrap();
        assert!(!index.has_unmerged_entries());
        let staged = index.get(b"f", 0).expect("stage 0");
        assert_eq!(repo.odb().read(&staged.oid).unwrap().data, b"base\nfeat\n");
    }

    #[test]
    fn stage_resolved_conflict_clears_unmerged_stages() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let ours = repo.odb().write(ObjectKind::Blob, b"ours\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"theirs\n").unwrap();

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f.txt", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f.txt", 2, ours, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f.txt", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);

        fs::write(root.join("f.txt"), b"resolved\n").unwrap();
        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        assert!(
            !index
                .entries
                .iter()
                .any(|e| e.path == b"f.txt" && e.stage() != 0),
            "unmerged stages must be cleared"
        );
        let staged = index.get(b"f.txt", 0).expect("stage 0 entry");
        let blob = repo.odb().read(&staged.oid).unwrap();
        assert_eq!(blob.data, b"resolved\n");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stage_linked_worktree_uses_common_repository_config() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&main)
            .status()
            .expect("git init")
            .success());
        for args in [
            ["config", "user.email", "t@example.com"],
            ["config", "user.name", "t"],
            ["config", "core.autocrlf", "true"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .unwrap();
        }
        std::fs::write(main.join("README"), b"seed\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "README"])
            .current_dir(&main)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&main)
            .status()
            .unwrap();

        let linked = root.join("linked");
        assert!(std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "linked-branch",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&main)
            .status()
            .expect("worktree add")
            .success());

        let repo =
            Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).unwrap();
        std::fs::write(linked.join("grit.txt"), b"grit\r\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        let oid = index.get(b"grit.txt", 0).unwrap().oid;
        assert_eq!(
            repo.odb().read(&oid).unwrap().data,
            b"grit\n",
            "linked worktree must use common repo autocrlf config"
        );
    }

    #[test]
    fn stage_applies_autocrlf_on_write() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        fs::write(root.join("f.txt"), b"changed\r\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        let oid = index.get(b"f.txt", 0).unwrap().oid;
        assert_eq!(repo.odb().read(&oid).unwrap().data, b"changed\n");
    }

    fn commit_in_repo(repo: &Repository, rel_file: &str, content: &[u8], msg: &str) -> ObjectId {
        let wt = repo.work_tree().unwrap();
        let abs = wt.join(rel_file);
        if let Some(p) = abs.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&abs, content).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, content).unwrap();
        let tree_body = format!("100644 {blob}\t{rel_file}\n");
        let tree = repo
            .odb()
            .write(ObjectKind::Tree, tree_body.as_bytes())
            .unwrap();
        let commit_body =
            format!("tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\n{msg}\n");
        repo.odb()
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .unwrap()
    }

    fn open_nested_repo(nested_root: &Path) -> Repository {
        let git = nested_root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        Repository::open(
            &git,
            &RepositoryOptions::empty().with_work_tree(nested_root),
        )
        .unwrap()
    }

    fn set_branch_head(repo: &Repository, head: ObjectId) {
        let main = repo.git_dir().join("refs/heads/main");
        fs::write(main, format!("{head}\n")).unwrap();
    }

    #[test]
    fn stage_all_records_unregistered_embedded_repo_as_gitlink() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a"), b"a\n").unwrap();
        let sub = root.join("inner");
        fs::create_dir_all(&sub).unwrap();
        let sub_repo = open_nested_repo(&sub);
        let head = commit_in_repo(&sub_repo, "i", b"i\n", "i");
        set_branch_head(&sub_repo, head);

        stage(
            &repo,
            &StageOptions {
                pathspecs: Vec::new(),
                pathspec_sources: Vec::new(),
                mode: StageMode::All,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        let entry = index.get(b"inner", 0).expect("gitlink at inner");
        assert_eq!(entry.mode, MODE_GITLINK);
        assert_eq!(entry.oid, head);
        assert!(index.get(b"inner/i", 0).is_none());
    }

    fn seed_one_file_dirs(root: &Path, count: usize) {
        for i in 0..count {
            let dir = root.join(format!("d{i}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
        }
    }

    fn stage_with_pathspecs(repo: &Repository, specs: &[&str]) -> StageOutcome {
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        stage(
            repo,
            &StageOptions {
                pathspecs: specs.clone(),
                pathspec_sources: specs,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap()
    }

    fn stage0_entries(repo: &Repository) -> Vec<(Vec<u8>, u32, ObjectId)> {
        repo.load_index()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.mode, e.oid))
            .collect()
    }

    #[test]
    fn stage_dot_on_many_untracked_dirs_matches_no_pathspec() {
        for tracked in [false, true] {
            let mut results = Vec::new();
            for specs in [&[][..], &["."], &["./"]] {
                let tmp = TempDir::new().unwrap();
                let root = tmp.path();
                let repo = init_repo(root);
                if tracked {
                    let mut index = Index::new();
                    stage_file_in_index(&repo, &mut index, "tracked/t.txt", b"t\n");
                    write_index(&repo, &mut index);
                }
                seed_one_file_dirs(root, 60);
                let outcome = stage_with_pathspecs(&repo, specs);
                assert_eq!(outcome.added.len(), 60, "{specs:?}");
                results.push(stage0_entries(&repo));
            }
            assert_eq!(results[0], results[1], "tracked={tracked}");
            assert_eq!(results[0], results[2], "tracked={tracked}");
        }
    }

    #[test]
    fn stage_pathspec_lists_each_untracked_dir_once() {
        const DIRS: u32 = 200;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"t\n");
        write_index(&repo, &mut index);
        seed_one_file_dirs(root, DIRS as usize);

        for specs in [&["*.txt"][..], &["d*"], &["."]] {
            crate::porcelain::status::untracked_walk_probe::reset();
            let outcome = stage(
                &repo,
                &StageOptions {
                    pathspecs: specs.iter().map(|s| (*s).to_owned()).collect(),
                    dry_run: true,
                    ..StageOptions::default()
                },
                &mut NullProgress,
            )
            .unwrap();
            assert_eq!(outcome.added.len(), DIRS as usize, "{specs:?}");
            let dirs_read = crate::porcelain::status::untracked_walk_probe::dirs_read();
            assert!(
                dirs_read <= DIRS + 8,
                "{specs:?}: untracked walk listed {dirs_read} directories for {DIRS} \
                 untracked directories"
            );
        }
    }

    #[test]
    fn stage_pathspec_keeps_embedded_repo_and_skips_ignored_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("top/a/build")).unwrap();
        fs::write(root.join("top/a/build/out.bin"), b"o\n").unwrap();
        fs::write(root.join("top/a/keep.txt"), b"k\n").unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("other/o.txt"), b"o\n").unwrap();
        let sub = root.join("top/inner");
        fs::create_dir_all(&sub).unwrap();
        let sub_repo = open_nested_repo(&sub);
        let head = commit_in_repo(&sub_repo, "i", b"i\n", "i");
        set_branch_head(&sub_repo, head);

        stage_with_pathspecs(&repo, &["top"]);

        let index = repo.load_index().unwrap();
        let gitlink = index.get(b"top/inner", 0).expect("gitlink at top/inner");
        assert_eq!(gitlink.mode, MODE_GITLINK);
        assert_eq!(gitlink.oid, head);
        assert!(index.get(b"top/a/keep.txt", 0).is_some());
        assert!(index.get(b"top/a/build/out.bin", 0).is_none());
        assert!(index.get(b"top/inner/i", 0).is_none());
        assert!(index.get(b"other/o.txt", 0).is_none());
    }

    #[test]
    fn stage_gitlink_records_submodule_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let sub_repo = open_nested_repo(&sub);
        let old_head = commit_in_repo(&sub_repo, "inside.txt", b"old\n", "old");
        set_branch_head(&sub_repo, old_head);

        let mut index = Index::new();
        let mut gitlink = conflict_index_entry("sub", 0, old_head, MODE_GITLINK);
        gitlink.size = 0;
        index.push_entry_unsorted(gitlink);
        write_index(&repo, &mut index);

        let new_head = commit_in_repo(&sub_repo, "inside.txt", b"new\n", "new");
        set_branch_head(&sub_repo, new_head);

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["sub".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        let entry = index.get(b"sub", 0).unwrap();
        assert_eq!(entry.mode, MODE_GITLINK);
        assert_eq!(entry.oid, new_head);
    }

    #[test]
    fn stage_update_ignores_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "tracked.txt", b"old\n");
        write_index(&repo, &mut index);
        fs::write(root.join("tracked.txt"), b"new\n").unwrap();
        fs::write(root.join("brand-new.txt"), b"hi\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified.len(), 1);
        assert_eq!(outcome.added.len(), 0);
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked.txt", 0).is_some());
        assert!(index.get(b"brand-new.txt", 0).is_none());
    }

    fn count_loose_objects(root: &Path) -> usize {
        let objects = root.join(".git/objects");
        let mut count = 0usize;
        for shard in fs::read_dir(&objects).unwrap() {
            let shard = shard.unwrap();
            if !shard.file_type().unwrap().is_dir() {
                continue;
            }
            let name = shard.file_name();
            if name == "info" || name == "pack" {
                continue;
            }
            count += fs::read_dir(shard.path()).unwrap().count();
        }
        count
    }

    #[test]
    fn stage_dry_run_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("tracked.txt"), b"v1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        fs::write(root.join("tracked.txt"), b"v2\n").unwrap();
        fs::write(root.join("new.txt"), b"n\n").unwrap();
        let index_before = repo.load_index().unwrap();
        let mtime_before = fs::metadata(repo.index_path()).unwrap().modified().ok();
        let objects_before = count_loose_objects(root);

        let outcome = stage(
            &repo,
            &StageOptions {
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert!(!outcome.added.is_empty() || !outcome.modified.is_empty());
        let index_after = repo.load_index().unwrap();
        assert_eq!(index_before.entries, index_after.entries);
        if let Some(before) = mtime_before {
            let after = fs::metadata(repo.index_path()).unwrap().modified().ok();
            assert_eq!(Some(before), after);
        }
        assert_eq!(
            count_loose_objects(root),
            objects_before,
            "dry-run must not write loose objects"
        );
        stage(
            &repo,
            &StageOptions {
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(count_loose_objects(root), objects_before);
    }

    #[test]
    fn add_update_resolves_unmerged_pathless() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"base\nfeat\n").unwrap();

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);
        fs::write(root.join("f"), b"base\nfeat\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.total(), 1, "pathless -u must stage resolved file");

        let index = repo.load_index().unwrap();
        assert!(
            !index.has_unmerged_entries(),
            "pathless -u must clear unmerged stages"
        );
        assert!(index.get(b"f", 0).is_some());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn add_update_unmerged_deletion_pathless() {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("f"), b"base\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"base\nfeat\n").unwrap();
        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        write_index(&repo, &mut index);
        let _ = fs::remove_file(root.join("f"));

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.removed, vec!["f".to_owned()]);
        assert!(outcome.modified.is_empty());
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        assert!(
            !index.has_unmerged_entries(),
            "deletion resolution must drop all conflict stages"
        );
        assert!(index.get(b"f", 0).is_none());

        let unmerged_lines: Vec<String> = String::from_utf8(
            std::process::Command::new("git")
                .args(["ls-files", "-u"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
        assert!(
            unmerged_lines.is_empty(),
            "git ls-files -u after grit add -u: {unmerged_lines:?}"
        );

        let tmp2 = TempDir::new().unwrap();
        let root2 = tmp2.path();
        grit_test_support::git(root2, &["init", "-q", "-b", "main"]);
        grit_test_support::git(root2, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root2, &["config", "user.name", "Test"]);
        fs::write(root2.join("f"), b"base\n").unwrap();
        grit_test_support::git(root2, &["add", "f"]);
        grit_test_support::git(root2, &["commit", "-qm", "seed"]);
        let repo2 = Repository::discover(Some(root2), &RepositoryOptions::empty()).unwrap();
        let mut index2 = Index::new();
        index2
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index2
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        write_index(&repo2, &mut index2);
        let _ = fs::remove_file(root2.join("f"));
        grit_test_support::git(root2, &["add", "-u"]);
        let git_unmerged: Vec<String> = String::from_utf8(
            std::process::Command::new("git")
                .args(["ls-files", "-u"])
                .current_dir(root2)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
        assert!(
            git_unmerged.is_empty(),
            "git add -u reference must clear unmerged entries"
        );
    }

    #[test]
    fn stage_outcome_lists_paths_bulk_and_serial() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"a\n").unwrap();
        fs::write(root.join("b.txt"), b"b\n").unwrap();
        let bulk = stage(
            &repo,
            &StageOptions {
                staging_thresholds: Some(IndexStagingThresholds {
                    large_index_restage: 0,
                    parallel_tracked_restage: 0,
                }),
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(bulk.added.len(), 2);
        assert!(bulk.added.contains(&"a.txt".to_owned()));
        assert!(bulk.added.contains(&"b.txt".to_owned()));

        fs::write(root.join("c.txt"), b"c\n").unwrap();
        let serial = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["c.txt".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(serial.added, vec!["c.txt".to_owned()]);
    }

    fn git_ls_files_s(repo: &Repository) -> Vec<String> {
        let wt = repo.work_tree().unwrap();
        let out = std::process::Command::new("git")
            .args(["ls-files", "-s"])
            .current_dir(wt)
            .output()
            .expect("git ls-files");
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn add_update_deleted_file() {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("keep.txt"), b"k\n").unwrap();
        fs::write(root.join("gone.txt"), b"g\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::write(root.join("keep.txt"), b"k2\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert!(outcome.removed.iter().any(|p| p == "gone.txt"));
        assert!(outcome.modified.iter().any(|p| p == "keep.txt"));

        let grit_lines = git_ls_files_s(&repo);
        grit_test_support::git(root, &["reset"]);
        grit_test_support::git(root, &["add", "-u"]);
        let git_lines: Vec<String> = String::from_utf8(
            std::process::Command::new("git")
                .args(["ls-files", "-s"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
        assert_eq!(grit_lines, git_lines);
        assert!(std::process::Command::new("git")
            .arg("fsck")
            .current_dir(root)
            .status()
            .expect("git fsck")
            .success());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn add_update_subdirectory_pathspec() {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), b"1\n").unwrap();
        fs::write(root.join("top.txt"), b"t\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        fs::write(root.join("sub/a.txt"), b"2\n").unwrap();
        fs::write(root.join("top.txt"), b"T\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                pathspecs: vec!["sub".to_owned()],
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified, vec!["sub/a.txt".to_owned()]);
        assert!(!outcome.modified.iter().any(|p| p == "top.txt"));
    }

    #[test]
    fn add_update_empty_pathspecs_whole_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("f.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        fs::write(root.join("f.txt"), b"2\n").unwrap();
        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::Update,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.modified.len(), 1);
        assert_eq!(outcome.added.len(), 0);
    }

    #[test]
    fn add_dry_run_lists_paths_without_writing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("f.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        fs::write(root.join("f.txt"), b"2\n").unwrap();
        fs::write(root.join("n.txt"), b"n\n").unwrap();
        let outcome = stage(
            &repo,
            &StageOptions {
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert!(outcome.modified.contains(&"f.txt".to_owned()));
        assert!(outcome.added.contains(&"n.txt".to_owned()));
        let index = repo.load_index().unwrap();
        assert!(index.get(b"n.txt", 0).is_none());
    }

    #[test]
    fn force_stages_named_ignored_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("a.log"), b"x\n").unwrap();

        let outcome = stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["a.log".to_owned()],
                pathspec_sources: vec!["a.log".to_owned()],
                force_ignored: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert_eq!(outcome.added, vec!["a.log".to_owned()]);
        let index = repo.load_index().unwrap();
        assert!(index.get(b"a.log", 0).is_some());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn intent_to_add_entry_round_trips_with_git() {
        use std::process::Command;

        if !Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("f.txt"), b"hello\n").unwrap();

        stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["f.txt".to_owned()],
                pathspec_sources: vec!["f.txt".to_owned()],
                intent_to_add: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        let ie = index.get(b"f.txt", 0).expect("ita row");
        assert!(ie.intent_to_add());
        assert_eq!(ie.oid, empty_blob_oid());

        let fsck = Command::new("git")
            .args(["fsck", "--strict"])
            .current_dir(root)
            .output()
            .expect("git fsck");
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stderr)
        );

        let diff_stat = Command::new("git")
            .args(["diff", "--stat"])
            .current_dir(root)
            .output()
            .expect("git diff --stat");
        assert!(diff_stat.status.success());
        assert!(String::from_utf8_lossy(&diff_stat.stdout).contains("f.txt"));

        let commit = Command::new("git")
            .args(["commit", "-a", "-m", "ita"])
            .current_dir(root)
            .output()
            .expect("git commit -a");
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );

        fs::write(root.join("g.txt"), b"other\n").unwrap();
        stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["g.txt".to_owned()],
                pathspec_sources: vec!["g.txt".to_owned()],
                intent_to_add: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        let grit_porcelain = Command::new("git")
            .args(["status", "--porcelain=v2"])
            .current_dir(root)
            .output()
            .unwrap();

        let tmp2 = TempDir::new().unwrap();
        let root2 = tmp2.path();
        grit_test_support::git(root2, &["init", "-q"]);
        grit_test_support::git(root2, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root2, &["config", "user.name", "Test"]);
        fs::write(root2.join("g.txt"), b"other\n").unwrap();
        grit_test_support::git(root2, &["add", "-N", "g.txt"]);
        let git_porcelain = Command::new("git")
            .args(["status", "--porcelain=v2"])
            .current_dir(root2)
            .output()
            .unwrap();

        fn porcelain_ita_lines(out: &[u8]) -> Vec<String> {
            String::from_utf8_lossy(out)
                .lines()
                .filter(|l| l.contains(" g.txt") || l.ends_with("g.txt"))
                .map(str::to_owned)
                .collect()
        }
        assert_eq!(
            porcelain_ita_lines(&grit_porcelain.stdout),
            porcelain_ita_lines(&git_porcelain.stdout),
            "porcelain v2 for -N must match git"
        );
    }

    fn plain_index_entry(path: &str) -> IndexEntry {
        plain_index_entry_path(path.as_bytes())
    }

    fn plain_index_entry_path(path: &[u8]) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::zero(),
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn sort_outcome_paths_in_index_order_non_utf8_matches_lossy_outcome_keys() {
        let mut index = Index::new();
        index.add_or_replace(plain_index_entry_path(b"a\x80.c"));
        index.add_or_replace(plain_index_entry("aé.c"));
        index.sort();

        let invalid_display = index_path_to_string(b"a\x80.c");
        let valid_display = index_path_to_string("aé.c".as_bytes());
        assert_ne!(invalid_display, valid_display);

        let mut outcome = StageOutcome {
            added: Vec::new(),
            modified: vec![valid_display.clone(), invalid_display.clone()],
            removed: Vec::new(),
            intent_to_add: Vec::new(),
        };
        sort_outcome_paths_in_index_order(&mut outcome, &index);
        assert_eq!(
            outcome.modified,
            vec![invalid_display, valid_display],
            "order must follow index entry order using lossy outcome strings"
        );
    }

    #[test]
    fn sort_outcome_paths_in_index_order_scales_with_index_build_not_sort_size() {
        const INDEX_LEN: usize = 30_000;
        const SORT_LEN: usize = 2_000;
        let mut index = Index::new();
        for i in 0..INDEX_LEN {
            index.add_or_replace(plain_index_entry(&format!("f/{i:06}.txt")));
        }
        index.sort();

        let modified: Vec<String> = (0..SORT_LEN)
            .map(|i| format!("f/{:06}.txt", INDEX_LEN - 1 - i))
            .collect();
        let mut outcome = StageOutcome {
            added: Vec::new(),
            modified,
            removed: Vec::new(),
            intent_to_add: Vec::new(),
        };

        let start = std::time::Instant::now();
        sort_outcome_paths_in_index_order(&mut outcome, &index);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 2,
            "sorting {SORT_LEN} paths against {INDEX_LEN} index entries took {elapsed:?}"
        );

        for window in outcome.modified.windows(2) {
            let pos_a = index
                .entries
                .iter()
                .position(|e| e.path == window[0].as_bytes())
                .expect("path in index");
            let pos_b = index
                .entries
                .iter()
                .position(|e| e.path == window[1].as_bytes())
                .expect("path in index");
            assert!(pos_a < pos_b);
        }
    }

    #[test]
    fn bulk_stage_and_parallel_restage_write_through_custom_primary() {
        use crate::odb::store::{MemoryStore, ObjectStore};
        use crate::odb::OdbBuilder;
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);
        let git = root.join(".git");
        let store = Arc::new(MemoryStore::new(crate::objects::HashAlgo::Sha1));
        let repo = crate::repo::RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(
                OdbBuilder::files(git.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .open(&git, Some(root))
            .unwrap();
        let bulk = StageOptions {
            staging_thresholds: Some(IndexStagingThresholds {
                large_index_restage: 0,
                parallel_tracked_restage: 0,
            }),
            ..StageOptions::default()
        };
        let paths: Vec<String> = (0..40).map(|i| format!("f{i:02}.txt")).collect();
        for path in &paths {
            fs::write(root.join(path), b"v1\n").unwrap();
        }
        assert_eq!(
            stage(&repo, &bulk, &mut NullProgress).unwrap().added.len(),
            paths.len()
        );

        for (i, path) in paths.iter().enumerate() {
            fs::write(root.join(path), format!("version two of {i}\n")).unwrap();
        }
        let restaged = stage(&repo, &bulk, &mut NullProgress).unwrap();
        assert_eq!(restaged.modified.len(), paths.len());

        let index = repo.load_index().unwrap();
        for (i, path) in paths.iter().enumerate() {
            let oid = index.get(path.as_bytes(), 0).unwrap().oid;
            let obj = store.read(&oid).unwrap().expect("blob in custom primary");
            assert_eq!(obj.data, format!("version two of {i}\n").into_bytes());
        }
        assert_eq!(
            fs::read_dir(git.join("objects")).unwrap().count(),
            0,
            "no loose shards under objects/"
        );
    }
}

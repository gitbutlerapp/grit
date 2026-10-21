//! Stage working-tree changes into the index (`git add`).
//!
//! [`stage`] performs a single index-vs-worktree scan (plus an untracked walk when
//! [`StageMode::All`] is selected). It does **not** compute the index-vs-HEAD diff
//! that full [`super::status::status`] requires.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use crate::diff::{
    is_unregistered_embedded_git_repo, materialize_worktree_blob, mode_from_metadata,
    read_submodule_head_oid, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::hash::{try_par_hash_with, ParallelHashError, Parallelism};
use crate::ignore::IgnoreMatcher;
use crate::index::{
    entry_from_metadata, index_file_mtime, Index, IndexEntry, MODE_GITLINK, MODE_TREE,
};
use crate::objects::ObjectId;
use crate::odb::BULK_NEW_LOOSE_WRITE;
use crate::path_icase::{paths_equal, worktree_path_for_index_entry, Stage0IcasePathMap};
use crate::pathspec::{has_glob_chars, pathspec_is_exclude, sorted_paths_match_pathspec};
use crate::porcelain::bulk_index::IndexStagingThresholds;
use crate::porcelain::index_mode::{
    apply_executable_hint_to_index, staged_mode_with_hint, StageExecutableHint,
};
use crate::porcelain::pathspec_match::{
    pathspec_matches_known_in_repository, sorted_head_tree_paths,
};
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
use crate::worktree_batch::{prepare_worktree_blobs_parallel, WorktreeBlobReadInput};
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
    /// When true, classify paths that would be staged without writing blobs, the index, or UNTR.
    pub dry_run: bool,
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
}

impl StageOutcome {
    /// Total number of index paths updated.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added
            .len()
            .saturating_add(self.modified.len())
            .saturating_add(self.removed.len())
    }
}

fn index_path_to_string(path: &[u8]) -> String {
    std::str::from_utf8(path)
        .map(str::to_owned)
        .unwrap_or_else(|_| String::from_utf8_lossy(path).into_owned())
}

fn sort_outcome_paths_in_index_order(outcome: &mut StageOutcome, index: &Index) {
    let mut order: BTreeMap<String, usize> = BTreeMap::new();
    for (i, entry) in index.entries.iter().enumerate() {
        if entry.stage() == 0 {
            order.entry(index_path_to_string(&entry.path)).or_insert(i);
        }
    }
    let sort_paths = |paths: &mut Vec<String>| {
        paths.sort_by(|a, b| {
            let ia = order.get(a).copied().unwrap_or(usize::MAX);
            let ib = order.get(b).copied().unwrap_or(usize::MAX);
            ia.cmp(&ib).then_with(|| a.as_str().cmp(b))
        });
        paths.dedup();
    };
    sort_paths(&mut outcome.added);
    sort_paths(&mut outcome.modified);
    sort_paths(&mut outcome.removed);
}

fn sort_outcome_paths_lexicographic(outcome: &mut StageOutcome) {
    for paths in [
        &mut outcome.added,
        &mut outcome.modified,
        &mut outcome.removed,
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

/// Like [`stage_for_commit`], but updates a caller-owned index and does not write it to disk.
///
/// Used by [`crate::porcelain::commit::commit_worktree`] to avoid reloading the index.
pub(crate) fn stage_for_commit_on_index(
    repo: &Repository,
    index: &mut Index,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    let opts = StageOptions {
        mode: StageMode::All,
        ..StageOptions::default()
    };
    stage_inner_on_index(repo, &opts, progress, true, index, false)
}

/// Stage paths matching `opts` into the index.
///
/// Loads the index, compares it to the work tree once, hashes only changed or new
/// blobs, removes deleted paths, and invalidates cache-tree / untracked-cache entries
/// for touched paths via [`Index::stage_file`] / [`Index::remove`].
///
/// # Errors
///
/// Returns [`Error::Message`] when an explicit pathspec matches nothing,
/// [`Error::PathspecIgnored`] when a path exists but is excluded by ignore rules,
/// or I/O / object-store failures while reading the work tree.
pub fn stage(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    if opts.pathspecs.is_empty() && opts.mode == StageMode::All && !opts.dry_run {
        return stage_with_tracked_scan(repo, opts, progress);
    }
    stage_via_worktree_diff(repo, opts, progress)
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

/// Match the UNTR `dir_flags` already stored on the index so add does not rebuild the cache.
fn untracked_show_all_matching_index(index: &Index) -> bool {
    index
        .untracked_cache
        .as_ref()
        .is_some_and(|uc| uc.dir_flags == 0)
}

fn stage_inner(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
    tracked_scan: bool,
) -> Result<StageOutcome> {
    let mut index = repo.load_index()?;
    stage_inner_on_index(repo, opts, progress, tracked_scan, &mut index, true)
}

fn stage_inner_on_index(
    repo: &Repository,
    opts: &StageOptions,
    _progress: &mut dyn ProgressSink,
    tracked_scan: bool,
    index: &mut Index,
    write_index: bool,
) -> Result<StageOutcome> {
    let dry_run = opts.dry_run;
    let persist_index = write_index && !dry_run;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

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

    let staging_thresholds = opts.staging_thresholds.unwrap_or_default();
    let use_tracked_scan = tracked_scan && !index.entries.is_empty();
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
            ..DiffIndexToWorktreeOptions::default()
        };
        let (unstaged, _, staging_stats) = crate::diff::diff_index_to_worktree_for_staging(
            repo.odb(),
            repo.git_dir(),
            index,
            work_tree,
            diff_opts,
        )?;
        (unstaged, staging_stats)
    };

    let rules = lock_rules(&rules_arc)?;
    let bulk_empty_index_add =
        opts.mode == StageMode::All && index.entries.is_empty() && opts.pathspecs.is_empty();
    let untracked = if opts.mode == StageMode::All {
        if bulk_empty_index_add {
            let scan = UntrackedScan {
                ignored_mode: IgnoredMode::No,
                show_all: true,
                sort_paths: false,
                use_untracked_cache: false,
                cache_miss: UntrackedCacheMissAction::RefreshFull,
            };
            let (untracked, _, _) = collect_untracked_and_ignored_inner(
                repo,
                index,
                work_tree,
                &opts.pathspecs,
                scan,
                &rules,
            )?;
            untracked
        } else {
            let scan = UntrackedScan {
                ignored_mode: IgnoredMode::No,
                show_all: untracked_show_all_matching_index(index),
                sort_paths: !use_tracked_scan,
                use_untracked_cache: true,
                cache_miss: UntrackedCacheMissAction::RefreshFull,
            };
            let (untracked, _, _) = collect_untracked_and_ignored_inner(
                repo,
                index,
                work_tree,
                &opts.pathspecs,
                scan,
                &rules,
            )?;
            if untracked.is_empty() {
                untracked
            } else {
                expand_untracked_for_staging_with_rules(
                    repo,
                    index,
                    work_tree,
                    untracked,
                    &opts.pathspecs,
                    &rules,
                )?
            }
        }
    } else {
        Vec::new()
    };

    if !opts.pathspecs.is_empty() {
        validate_pathspecs(repo, index, work_tree, &unstaged, &untracked, opts, &rules)?;
    }

    let matches =
        |path: &str| opts.pathspecs.is_empty() || repo.paths_match_pathspecs(path, &opts.pathspecs);

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
            if has_unmerged && unmerged_only_paths.contains(path.as_bytes()) {
                continue;
            }
            if plan.remove {
                if dry_run {
                    if index.get(path.as_bytes(), 0).is_some() {
                        outcome.removed.push(path);
                    }
                } else if index.remove(path.as_bytes()) {
                    outcome.removed.push(path);
                }
                continue;
            }
            if !dry_run {
                icase_map.remove_alias_of(index, path.as_bytes());
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
            }
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
            &mut outcome,
        )?;
    }

    if has_unmerged && !opts.pathspecs.is_empty() {
        outcome
            .modified
            .extend(stage_unmerged_paths_matching_pathspecs(
                repo, work_tree, &matches, index, &rules, dry_run,
            )?);
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
        if persist_index {
            repo.write_index(index)?;
        }
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
) -> Result<Vec<String>> {
    let mut unmerged: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            unmerged.insert(entry.path.clone(), ());
        }
    }
    let mut updated = Vec::new();
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
            Err(_) => continue,
        };
        if dry_run {
            updated.push(rel.to_owned());
            continue;
        }
        let file_attrs = rules.file_attrs(rel, false);
        let mode = mode_from_metadata(&meta);
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
        mark_fsmonitor_staged(index, rel);
        updated.push(rel.to_owned());
    }
    Ok(updated)
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

    for raw_path in stage0 {
        let index_rel = match std::str::from_utf8(&raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !matches(index_rel) {
            continue;
        }
        let abs = worktree_path_for_index_entry(work_tree, index_rel, true);
        let Ok(rel) = abs.strip_prefix(work_tree) else {
            continue;
        };
        let actual_rel = rel.to_string_lossy().replace('\\', "/");
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
            cached_meta: None,
        });
    }

    let prepared = prepare_worktree_blobs_parallel(
        repo.odb(),
        &batch_inputs,
        rules.conversion(),
        parallelism,
        Some(rules.filter_process()),
    )?;

    if !dry_run && !prepared.is_empty() {
        repo.odb().ensure_all_loose_prefix_dirs()?;
    }
    let write_opts = BULK_NEW_LOOSE_WRITE;
    let write_bytes: usize = prepared.iter().map(|p| p.zlib_store.len()).sum();
    let threads = parallelism.threads();
    if !dry_run {
        let _written: Vec<ObjectId> = try_par_hash_with(&prepared, threads, write_bytes, |prep| {
            repo.odb()
                .write_loose_zlib_prehashed(&prep.oid, &prep.zlib_store, write_opts)
                .map_err(|e| match e {
                    Error::Io(source) => Error::LooseObjectWrite {
                        path: prep.index_relpath.clone(),
                        source,
                    },
                    other => other,
                })
        })
        .map_err(|e: ParallelHashError<Error>| match e {
            ParallelHashError::Task(err) => err,
        })?;
    }

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
        if dry_run {
            outcome.added.push(prep.index_relpath);
            continue;
        }
        icase_map.remove_alias_of(index, prep.index_relpath.as_bytes());
        let mode =
            mode_with_executable_hint(prep.mode, &prep.index_relpath, executable_hint, matches);
        let mut entry =
            entry_from_metadata(&prep.meta, prep.index_relpath.as_bytes(), prep.oid, mode);
        entry.mode = mode;
        if !skip_per_path_cache_invalidate {
            index.invalidate_untracked_cache_for_path(&prep.index_relpath);
            index.invalidate_cache_tree_for_path(&entry.path);
        }
        batch_entries.push(entry);
        outcome.added.push(prep.index_relpath);
    }
    let staged_batch_count = batch_entries.len();
    if !dry_run {
        for entry in batch_entries {
            if let Ok(rel) = std::str::from_utf8(&entry.path) {
                mark_fsmonitor_staged(index, rel);
            }
            index.add_or_replace(entry);
        }
        if staged_batch_count > 0 && index.fsmonitor_last_update.is_some() {
            index.fsmonitor_validity_changed();
        }
    }

    for path in deferred {
        if dry_run {
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
            matches,
        )?;
        outcome.added.push(path);
    }

    Ok(())
}

fn stage_untracked_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    rules: &WorktreeRules,
    index: &mut Index,
    executable_hint: StageExecutableHint,
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
    let oid = materialize_worktree_blob(
        repo.odb(),
        &abs,
        &meta,
        rules.conversion(),
        &file_attrs,
        &index_relpath,
        None,
        Some(rules.filter_process()),
    )
    .map_err(|e| Error::Message(format!("could not store {index_relpath}: {e}")))?;

    let mut entry = entry_from_metadata(&meta, index_relpath.as_bytes(), oid, mode);
    entry.mode = mode;
    index.stage_file(entry);
    mark_fsmonitor_staged(index, &index_relpath);
    Ok(())
}

fn validate_pathspecs(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    unstaged: &[DiffEntry],
    untracked: &[String],
    opts: &StageOptions,
    rules: &WorktreeRules,
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
    let known = sorted_stageable_paths(index, unstaged, untracked);
    // Walking HEAD's tree is only needed for a pathspec that matches nothing else.
    let mut head_paths: Option<Vec<String>> = None;
    let mut ignored_paths = Vec::new();
    for (label, resolved) in positive {
        if sorted_paths_match_pathspec(resolved, &known, ctx) {
            continue;
        }
        if head_paths.is_none() {
            head_paths = Some(sorted_head_tree_paths(repo)?);
        }
        let head = head_paths.as_deref().unwrap_or_default();
        if sorted_paths_match_pathspec(resolved, head, ctx) {
            continue;
        }
        if !pathspec_matches_known_in_repository(repo, resolved)? {
            return Err(Error::Message(format!(
                "pathspec '{label}' did not match any files"
            )));
        }
        if let Some(display) =
            ignored_display_for_explicit_add(repo, index, rules, work_tree, label, resolved)?
        {
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

fn ignored_display_for_explicit_add(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    work_tree: &Path,
    label: &str,
    resolved: &str,
) -> Result<Option<String>> {
    if pathspec_is_exclude(resolved)
        || has_glob_chars(resolved)
        || resolved.starts_with(":(")
        || resolved.starts_with(':')
    {
        return Ok(None);
    }
    let precompose = repo.pathspec_precompose_enabled();
    let staged = resolve_worktree_path_for_staging(work_tree, resolved, precompose);
    if !staged.abs.exists() {
        return Ok(None);
    }
    let rel = staged.index_relpath;
    let is_dir = staged.abs.is_dir();
    let mut ignore = rules.ignore_mut();
    if is_dir {
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

fn pathspec_sources(opts: &StageOptions) -> Vec<String> {
    if opts.pathspec_sources.is_empty() {
        opts.pathspecs.clone()
    } else {
        opts.pathspec_sources.clone()
    }
}

/// Index (all stages), unstaged and untracked paths in byte order, borrowed where possible.
fn sorted_stageable_paths<'a>(
    index: &'a Index,
    unstaged: &'a [DiffEntry],
    untracked: &'a [String],
) -> Vec<Cow<'a, str>> {
    let mut paths: Vec<Cow<'a, str>> =
        Vec::with_capacity(index.entries.len() + unstaged.len() + untracked.len());
    paths.extend(
        index
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path)),
    );
    paths.extend(unstaged.iter().map(|e| Cow::Borrowed(e.path())));
    paths.extend(untracked.iter().map(|p| Cow::Borrowed(p.as_str())));
    paths.sort_unstable();
    paths.dedup();
    paths
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
        let objects_after = fs::read_dir(root.join(".git/objects")).unwrap().count();
        stage(
            &repo,
            &StageOptions {
                dry_run: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        let objects_again = fs::read_dir(root.join(".git/objects")).unwrap().count();
        assert_eq!(objects_after, objects_again);
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
}

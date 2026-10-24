//! `git status` as a structured operation.
//!
//! The library computes a [`StatusModel`] — every fact the user-facing output
//! needs, with **no presentation applied** — and the `grit` binary renders it
//! into porcelain v1/v2, short, or long format (applying colour, column layout,
//! path quoting, and the comment prefix). This is the reference example of the
//! library/CLI split described on [`crate::porcelain`].
//!
//! # Status of the extraction
//!
//! This module currently defines the data contract ([`StatusOptions`] in,
//! [`StatusModel`] out). The computation that produces the model is being moved
//! out of `grit/src/commands/status.rs::run` in stages; once it lands here as
//! [`status`], the three CLI formatters (`format_porcelain_v2`, `format_short`,
//! `format_long`) consume a `&StatusModel` instead of a dozen loose arguments.
//!
//! The model's shape is taken directly from the inputs those three formatters
//! share today: HEAD + its tree, the staged (index-vs-HEAD) and unstaged
//! (index-vs-worktree) diffs, the untracked and ignored path lists, the
//! in-progress operation [`state`](crate::state::WtStatusState), the loaded and
//! sparse-expanded index, and the stash count.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, DirEntry, ReadDir};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::diff::{is_unregistered_embedded_git_repo, DiffEntry};
use crate::error::ConfigError;
use crate::error::Result;
use crate::hash::{try_par_hash_with, ParallelHashError};
use crate::ignore::IgnoreMatcher;
use crate::index::{Index, MODE_GITLINK, MODE_TREE};
use crate::objects::ObjectId;
use crate::parallel::ParallelOp;
use crate::precompose_config::effective_core_precomposeunicode_with_config;
use crate::repo::Repository;
use crate::state::{status_operation, HeadState, Operation, WtStatusState};
use crate::unicode_normalization::precompose_utf8_segment;

/// How untracked files are reported (`git status --untracked-files=<mode>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedMode {
    /// `no` — do not list untracked files.
    No,
    /// `normal` — list untracked files and directories.
    Normal,
    /// `all` — list every individual untracked file, recursing into directories.
    All,
}

/// How ignored files are reported (`git status --ignored[=<mode>]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoredMode {
    /// Do not list ignored files (the default).
    No,
    /// `traditional` — list ignored files and directories.
    Traditional,
    /// `matching` — list only ignored paths that match an ignore pattern.
    Matching,
}

/// Rename/copy detection settings for the status diffs (`status.renames` /
/// `--find-renames`, `status.renameLimit`, copy detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameDetection {
    /// Rename similarity threshold, as a percentage (e.g. `50` for 50%).
    pub threshold: u32,
    /// Whether to also detect copies (`-C` / `--find-copies`).
    pub copies: bool,
}

/// Inputs that drive **what** `status` computes.
///
/// The CLI translates clap arguments and config (`status.showUntrackedFiles`,
/// `status.renames`, `status.aheadBehind`, submodule ignore settings, …) into
/// this plain struct. Presentation choices — short vs. porcelain vs. long,
/// colour, column layout, path quoting, `-z` — are deliberately absent; they
/// belong to the renderer, not the computation.
#[derive(Debug, Clone)]
pub struct StatusOptions {
    /// Untracked-file reporting mode.
    pub untracked: UntrackedMode,
    /// Ignored-file reporting mode.
    pub ignored: IgnoredMode,
    /// Rename/copy detection, or `None` to skip rename detection.
    pub renames: Option<RenameDetection>,
    /// Limit the report to paths matching these pathspecs (empty = whole tree).
    pub pathspecs: Vec<String>,
    /// Compute ahead/behind counts relative to the upstream branch.
    pub ahead_behind: bool,
    /// Override parallel index stat workers (`Some(1)` forces serial). Used by tests and embedders.
    #[doc(hidden)]
    pub stat_parallel_threads: Option<usize>,
    /// Hook protocol v1 query boundary for fsmonitor refresh (deterministic tests).
    #[doc(hidden)]
    pub fsmonitor_query_boundary: Option<crate::fsmonitor::FsmonitorQueryBoundary>,
    /// When `true`, count entries on `refs/stash` (reads the stash reflog).
    pub count_stash: bool,
}

/// Map `status.showUntrackedFiles` to [`UntrackedMode`].
///
/// Accepts Git's enum modes (`no`, `normal`, `all`, case-insensitive), boolean word
/// synonyms (since Git 2.45), and integer booleans via [`parse_git_config_maybe_bool`]
/// (same rules as Git's `parse_untracked_setting_name`).
/// When the key is missing, returns [`UntrackedMode::Normal`].
pub fn untracked_mode_from_config(
    config: &ConfigSet,
) -> std::result::Result<UntrackedMode, ConfigError> {
    let raw = config
        .get("status.showUntrackedFiles")
        .or_else(|| config.get("status.showuntrackedfiles"));
    match raw {
        None => Ok(UntrackedMode::Normal),
        Some(v) => untracked_mode_from_config_value(&v)
            .ok_or(ConfigError::InvalidUntrackedFilesMode { value: v }),
    }
}

fn untracked_mode_from_config_value(v: &str) -> Option<UntrackedMode> {
    let mapped = match parse_git_config_maybe_bool(v) {
        Some(false) => "no",
        Some(true) => "normal",
        None => v,
    };
    if mapped.eq_ignore_ascii_case("no") {
        Some(UntrackedMode::No)
    } else if mapped.eq_ignore_ascii_case("all") {
        Some(UntrackedMode::All)
    } else if mapped.eq_ignore_ascii_case("normal") {
        Some(UntrackedMode::Normal)
    } else {
        None
    }
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            untracked: UntrackedMode::Normal,
            ignored: IgnoredMode::No,
            renames: None,
            pathspecs: Vec::new(),
            ahead_behind: true,
            stat_parallel_threads: None,
            fsmonitor_query_boundary: None,
            count_stash: false,
        }
    }
}

/// Unmerged index conflict category (matches `git status --porcelain=v2` `u` XY codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    /// `UU` — both sides modified the same file.
    BothModified,
    /// `AA` — both sides added the same path.
    BothAdded,
    /// `DD` — both sides deleted the path.
    BothDeleted,
    /// `AU` — we added the path; they did not have it in the merge base.
    AddedByUs,
    /// `UA` — they added the path; we did not have it in the merge base.
    AddedByThem,
    /// `DU` — we deleted; they modified (or kept) the path.
    DeletedByUs,
    /// `UD` — we modified (or kept) the path; they deleted.
    DeletedByThem,
}

impl ConflictKind {
    /// Stable snake-case id for JSON and docs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BothModified => "both_modified",
            Self::BothAdded => "both_added",
            Self::BothDeleted => "both_deleted",
            Self::AddedByUs => "added_by_us",
            Self::AddedByThem => "added_by_them",
            Self::DeletedByUs => "deleted_by_us",
            Self::DeletedByThem => "deleted_by_them",
        }
    }
}

/// One unmerged path reported by status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Repository-relative path.
    pub path: String,
    /// How the two sides collided (from index stages 1–3).
    pub kind: ConflictKind,
}

/// Derive [`ConflictKind`] from which unmerged index stages exist for a path.
#[must_use]
pub fn conflict_kind_from_stages(stage1: bool, stage2: bool, stage3: bool) -> ConflictKind {
    conflict_kind_from_stagemask(stagemask_from_stages(stage1, stage2, stage3))
}

fn stagemask_from_stages(stage1: bool, stage2: bool, stage3: bool) -> u8 {
    let mut mask = 0u8;
    if stage1 {
        mask |= 1;
    }
    if stage2 {
        mask |= 2;
    }
    if stage3 {
        mask |= 4;
    }
    mask
}

/// Map Git's unmerged stage bitmask to [`ConflictKind`] (see upstream `wt-status.c`).
#[must_use]
pub fn conflict_kind_from_stagemask(stagemask: u8) -> ConflictKind {
    match stagemask {
        1 => ConflictKind::BothDeleted,
        2 => ConflictKind::AddedByUs,
        3 => ConflictKind::DeletedByThem,
        4 => ConflictKind::AddedByThem,
        5 => ConflictKind::DeletedByUs,
        6 => ConflictKind::BothAdded,
        7 => ConflictKind::BothModified,
        other => {
            debug_assert!(false, "unexpected unmerged stagemask {other:#x}");
            ConflictKind::BothModified
        }
    }
}

/// Default rename similarity threshold for `git status` when detection is enabled (`-M50`).
const STATUS_RENAME_DEFAULT_THRESHOLD: u32 = 50;

/// Build [`StatusOptions`] from repository config (untracked mode and rename detection).
///
/// CLI callers merge command-line overrides for [`StatusOptions::untracked`],
/// [`StatusOptions::pathspecs`], and other flags after calling this.
pub fn status_options_from_config(
    config: &ConfigSet,
) -> std::result::Result<StatusOptions, ConfigError> {
    Ok(StatusOptions {
        untracked: untracked_mode_from_config(config)?,
        renames: status_renames_from_config(config),
        ..StatusOptions::default()
    })
}

/// Resolve rename detection for status from `status.renames`, then `diff.renames`.
///
/// When both keys are absent, rename detection is **on** (Git default). Either key set to
/// `false`/`no`/`off`/`0` disables detection.
#[must_use]
pub fn status_renames_from_config(config: &ConfigSet) -> Option<RenameDetection> {
    if let Some(v) = config.get("status.renames") {
        return parse_status_renames_config_value(&v);
    }
    if let Some(v) = config.get("diff.renames") {
        return parse_status_renames_config_value(&v);
    }
    Some(RenameDetection {
        threshold: STATUS_RENAME_DEFAULT_THRESHOLD,
        copies: false,
    })
}

fn parse_status_renames_config_value(raw: &str) -> Option<RenameDetection> {
    let v = raw.trim();
    if v.eq_ignore_ascii_case("false")
        || v.eq_ignore_ascii_case("no")
        || v.eq_ignore_ascii_case("off")
        || v == "0"
    {
        return None;
    }
    let copies = v.eq_ignore_ascii_case("copy") || v.eq_ignore_ascii_case("copies");
    Some(RenameDetection {
        threshold: STATUS_RENAME_DEFAULT_THRESHOLD,
        copies,
    })
}

/// Whether a diff entry's primary path matches a pathspec list (pre-rename-detection filter).
#[must_use]
pub fn status_diff_entry_matches_pathspec(entry: &DiffEntry, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    status_path_matches(entry.path(), pathspecs)
}

/// Apply `status.renameLimit` over `diff.renameLimit` for rename scoring (Git status precedence).
fn config_for_status_rename_detection(config: &ConfigSet) -> ConfigSet {
    let mut out = config.clone();
    if let Some(limit) = config
        .get("status.renameLimit")
        .or_else(|| config.get("status.renamelimit"))
    {
        let _ = out.add_command_override("diff.renameLimit", &limit);
    }
    out
}

/// The computed result of `git status`: everything the renderers need, with no
/// presentation applied.
///
/// Fields mirror what the CLI's `format_porcelain_v2`, `format_short`, and
/// `format_long` read today, so a renderer can be a pure function of this model
/// plus the user's chosen output format.
#[derive(Debug, Clone)]
pub struct StatusModel {
    /// The resolved HEAD (branch, detached, or unborn).
    pub head: HeadState,
    /// Tree OID of the HEAD commit, or `None` on an unborn branch.
    pub head_tree: Option<ObjectId>,
    /// In-progress operation state (merge, rebase, cherry-pick, bisect, …).
    pub state: WtStatusState,
    /// The loaded index, with sparse-directory placeholders expanded — the same
    /// index the renderers query for per-stage entries.
    pub index: Index,
    /// Staged changes: the index-vs-HEAD-tree diff.
    pub staged: Vec<DiffEntry>,
    /// Unstaged changes: the index-vs-worktree diff.
    pub unstaged: Vec<DiffEntry>,
    /// Untracked paths (subject to [`StatusOptions::untracked`]).
    pub untracked: Vec<String>,
    /// Ignored paths (subject to [`StatusOptions::ignored`]).
    pub ignored: Vec<String>,
    /// Paths with unmerged index stages (1–3), sorted and deduplicated.
    pub conflicts: Vec<String>,
    /// Unmerged paths with a [`ConflictKind`] derived from index stages 1–3.
    pub conflict_details: Vec<Conflict>,
    /// Paused operation and suggested follow-up steps, when any apply.
    pub operation: Option<Operation>,
    /// Number of stash entries (for the optional stash footer / `--show-stash`).
    pub stash_count: usize,
    /// Whether the on-disk index used the sparse-directory format.
    pub index_sparse_on_disk: bool,
    /// Sparse-directory prefixes present in the on-disk index, if any.
    pub sparse_directory_prefixes: Vec<Vec<u8>>,
}

// --- Untracked / ignored worktree walk -------------------------------------
//
// Moved verbatim out of `grit/src/commands/status.rs` (Phase 4, step 2). This is
// pure domain logic: given the index + ignore rules, walk the work tree and
// produce the untracked and ignored path lists. The CLI's fsmonitor query,
// untracked-cache refresh, and trace2 emission wrap this — those stay in the CLI
// because they are IPC / env / optimization concerns, not status computation.

fn ignored_mode_to_untracked_cache(
    mode: IgnoredMode,
) -> crate::untracked_cache::UntrackedIgnoredMode {
    use crate::untracked_cache::UntrackedIgnoredMode;
    match mode {
        IgnoredMode::No => UntrackedIgnoredMode::No,
        IgnoredMode::Traditional => UntrackedIgnoredMode::Traditional,
        IgnoredMode::Matching => UntrackedIgnoredMode::Matching,
    }
}

/// Walk the work tree and collect untracked and ignored paths.
///
/// `ignored_mode` selects whether (and how) ignored paths are reported;
/// `show_all` corresponds to `--untracked-files=all`. Results are sorted.
pub fn collect_untracked_and_ignored(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    pathspecs: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    collect_untracked_and_ignored_with_cache(
        repo,
        index,
        work_tree,
        ignored_mode,
        show_all,
        pathspecs,
        true,
    )
}

/// Like [`collect_untracked_and_ignored`], with control over UNTR cache use.
///
/// When `use_untracked_cache` is false, the index UNTR extension is not refreshed (used by
/// staging/`git add`, which must not pay the full status-style cache rebuild).
pub fn collect_untracked_and_ignored_with_cache(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    pathspecs: &[String],
    use_untracked_cache: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    collect_untracked_and_ignored_inner(
        repo,
        index,
        work_tree,
        pathspecs,
        UntrackedScan {
            ignored_mode,
            show_all,
            sort_paths: true,
            use_untracked_cache,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        },
        &rules,
    )
    .map(|(untracked, ignored, _)| (untracked, ignored))
}

/// Like [`collect_untracked_and_ignored`], reusing an existing
/// [`crate::worktree_rules::WorktreeRules`] so ignore and attribute files already loaded by the
/// same operation are not read again.
pub fn collect_untracked_and_ignored_with_rules(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    pathspecs: &[String],
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<(Vec<String>, Vec<String>, UntrackedCollectMeta)> {
    collect_untracked_and_ignored_inner(
        repo,
        index,
        work_tree,
        pathspecs,
        UntrackedScan {
            ignored_mode,
            show_all,
            sort_paths: true,
            use_untracked_cache: true,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        },
        rules,
    )
}

/// Action when the UNTR fast path misses during an untracked walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UntrackedCacheMissAction {
    /// Rebuild the UNTR extension from the work tree (status default).
    #[default]
    RefreshFull,
}

/// How [`collect_untracked_and_ignored_inner`] walks and reports the work tree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UntrackedScan {
    /// Whether (and how) ignored paths are reported.
    pub ignored_mode: IgnoredMode,
    /// `--untracked-files=all`: list files inside untracked directories.
    pub show_all: bool,
    /// Sort the returned paths; unsorted results allow the parallel top-level walk.
    pub sort_paths: bool,
    /// Refresh and answer from the index UNTR extension when configured.
    pub use_untracked_cache: bool,
    /// How to recover when [`UntrackedScan::use_untracked_cache`] cannot answer from cache.
    pub cache_miss: UntrackedCacheMissAction,
}

/// Side effects from an untracked/ignored walk beyond the path lists.
#[derive(Debug, Clone, Copy, Default)]
pub struct UntrackedCollectMeta {
    /// The index UNTR extension was rebuilt and should be written to disk.
    pub untracked_cache_refreshed: bool,
}

/// Shared implementation of the untracked/ignored walk.
pub(crate) fn collect_untracked_and_ignored_inner(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    pathspecs: &[String],
    scan: UntrackedScan,
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<(Vec<String>, Vec<String>, UntrackedCollectMeta)> {
    let UntrackedScan {
        ignored_mode,
        show_all,
        sort_paths,
        use_untracked_cache,
        cache_miss,
    } = scan;
    // Keep parity with historical status behavior in tests that rely on broad untracked scans
    // (including detached-HEAD wtstatus cases): when no explicit pathspec is requested, avoid
    // pathspec-based pruning entirely.
    let effective_pathspecs: &[String] = if pathspecs.is_empty() { &[] } else { pathspecs };

    if use_untracked_cache && effective_pathspecs.is_empty() && ignored_mode == IgnoredMode::No {
        let config = rules.config_arc();
        let cache_on_index = index.untracked_cache.is_some();
        if cache_on_index
            && crate::untracked_cache::untracked_cache_enabled_for_status(config.as_ref(), index)
        {
            let ident = crate::untracked_cache::untracked_cache_ident(work_tree);
            let mut uc = index.untracked_cache.take().unwrap_or_else(|| {
                crate::untracked_cache::UntrackedCache::new_shell(0, ident.clone())
            });
            if uc.ident == ident {
                if let Some(untracked) = crate::untracked_cache::try_untracked_from_valid_cache(
                    repo,
                    work_tree,
                    config.as_ref(),
                    &mut uc,
                    show_all,
                )? {
                    index.untracked_cache = Some(uc);
                    return Ok((untracked, Vec::new(), UntrackedCollectMeta::default()));
                }
                if cache_miss == UntrackedCacheMissAction::RefreshFull {
                    crate::untracked_cache::refresh_untracked_cache_for_status(
                        repo,
                        index,
                        work_tree,
                        &config,
                        &mut uc,
                        show_all,
                        ignored_mode_to_untracked_cache(ignored_mode),
                    )?;
                    let untracked = crate::untracked_cache::collect_untracked_from_cache(&uc);
                    index.untracked_cache = Some(uc);
                    return Ok((
                        untracked,
                        Vec::new(),
                        UntrackedCollectMeta {
                            untracked_cache_refreshed: true,
                        },
                    ));
                }
                index.untracked_cache = Some(uc);
            } else {
                index.untracked_cache = Some(uc);
            }
        }
    }
    let ignorecase = rules
        .config()
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let tracked_paths =
        crate::path_icase::Stage0TrackedPaths::from_index_for_untracked_scan(index, ignorecase);

    let gitlinks: HashSet<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| crate::repo_relpath::storage_string(&e.path))
        .collect();

    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let precompose_unicode =
        effective_core_precomposeunicode_with_config(Some(repo.git_dir()), Some(rules.config()));

    if effective_pathspecs.is_empty() {
        let (mut par_untracked, mut par_ignored) = collect_untracked_parallel_top_level(
            repo,
            index,
            work_tree,
            &gitlinks,
            ignored_mode,
            show_all,
            precompose_unicode,
            &tracked_paths,
            &rules.ignore(),
            effective_pathspecs,
        )?;
        if sort_paths {
            par_untracked.sort_unstable();
            par_ignored.sort_unstable();
        }
        return Ok((par_untracked, par_ignored, UntrackedCollectMeta::default()));
    }

    visit_untracked_node(
        repo,
        index,
        work_tree,
        &gitlinks,
        &mut rules.ignore_mut(),
        ignored_mode,
        show_all,
        false,
        None::<&Cell<bool>>,
        precompose_unicode,
        &tracked_paths,
        "",
        work_tree,
        effective_pathspecs,
        Some(rules),
        &mut untracked,
        &mut ignored,
    )?;

    if sort_paths {
        untracked.sort();
        ignored.sort();
    }
    Ok((untracked, ignored, UntrackedCollectMeta::default()))
}

/// Expand collapsed untracked directory markers (`dir/`) into concrete file paths for staging.
///
/// [`collect_untracked_and_ignored`] with `show_all = false` reports directories as a single
/// `dir/` entry; `git add` must stage the non-ignored files inside instead of a tree placeholder.
pub fn expand_untracked_for_staging(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    untracked: Vec<String>,
    pathspecs: &[String],
) -> Result<Vec<String>> {
    if !untracked.iter().any(|p| p.ends_with('/')) {
        let mut files = untracked;
        files.sort();
        files.dedup();
        return Ok(files);
    }
    let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    expand_untracked_for_staging_with_rules(repo, index, work_tree, untracked, pathspecs, &rules)
}

/// Like [`expand_untracked_for_staging`], reusing an existing
/// [`crate::worktree_rules::WorktreeRules`] for every nested directory walk.
pub fn expand_untracked_for_staging_with_rules(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    untracked: Vec<String>,
    pathspecs: &[String],
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<Vec<String>> {
    let mut expanded = Vec::new();
    for path in untracked {
        if !path.ends_with('/') {
            expanded.push(path);
            continue;
        }
        let dir = path.trim_end_matches('/').to_owned();
        if dir.is_empty() {
            continue;
        }
        let abs_dir = work_tree.join(&dir);
        let mut ignore = rules.ignore().clone();
        if ignore.is_path_ignored(repo, Some(index), &dir, true)? {
            continue;
        }
        if is_unregistered_embedded_git_repo(index, &dir, &abs_dir) {
            expanded.push(format!("{dir}/"));
            continue;
        }
        let narrow = if pathspecs.is_empty() {
            vec![dir.clone()]
        } else {
            pathspecs.to_vec()
        };
        let (files, _, _) = collect_untracked_and_ignored_with_rules(
            repo,
            index,
            work_tree,
            IgnoredMode::No,
            true,
            &narrow,
            rules,
        )?;
        for file in files {
            if file.ends_with('/') {
                let nested = expand_untracked_for_staging_with_rules(
                    repo,
                    index,
                    work_tree,
                    vec![file],
                    &narrow,
                    rules,
                )?;
                expanded.extend(nested);
            } else {
                expanded.push(file);
            }
        }
    }
    expanded.sort();
    expanded.dedup();
    Ok(expanded)
}

/// Test-only counter of directory entries consumed during check-only untracked probes.
#[cfg(test)]
pub(crate) mod untracked_walk_probe {
    use std::cell::Cell;

    thread_local! {
        static ENTRIES: Cell<u32> = const { Cell::new(0) };
    }

    pub fn reset() {
        ENTRIES.with(|c| c.set(0));
    }

    pub fn count() -> u32 {
        ENTRIES.with(|c| c.get())
    }

    pub(super) fn record_entry() {
        ENTRIES.with(|c| c.set(c.get().saturating_add(1)));
    }
}

enum UntrackedWalkStep {
    Continue,
    Stop,
}

/// Parallel untracked scan without pathspecs, preserving path order loosely.
///
/// Work is split per top-level entry; on larger indexes, directories that only recurse are
/// split further (see [`split_tracked_untracked_tasks`]) so a work tree with a single
/// top-level directory still spreads across workers.
///
/// Only used without pathspecs, so the walk never needs attribute rules (`rules = None`); the
/// per-operation [`crate::worktree_rules::WorktreeRules`] is not shareable across workers.
#[allow(clippy::too_many_arguments)]
fn collect_untracked_parallel_top_level(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    ignored_mode: IgnoredMode,
    show_all: bool,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    matcher_template: &IgnoreMatcher,
    pathspecs: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let entries = named_entries_sorted(fs::read_dir(work_tree).map_err(crate::error::Error::Io)?);

    let mut root_files = Vec::new();
    let mut root_dirs = Vec::new();
    for named in entries {
        if named.0 == b".git" {
            continue;
        }
        if crate::read_dir_kind::entry_is_dir(&named.1).unwrap_or(false) {
            root_dirs.push(named);
        } else if !crate::read_dir_kind::entry_is_git_ignored_special(&named.1).unwrap_or(true) {
            root_files.push(named);
        }
    }

    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    let parallelism = repo
        .thread_policy()
        .resolve(ParallelOp::IndexRefresh, &config)
        .parallelism();
    let threads = parallelism.threads();

    if !root_files.is_empty() {
        let scan_root_file = |(name, entry): &(Vec<u8>, DirEntry),
                              local_untracked: &mut Vec<String>,
                              local_ignored: &mut Vec<String>| {
            let mut matcher = matcher_template.clone();
            let _ = visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                gitlinks,
                &mut matcher,
                ignored_mode,
                show_all,
                false,
                None::<&Cell<bool>>,
                precompose_unicode,
                tracked_paths,
                "",
                entry,
                name,
                pathspecs,
                None,
                local_untracked,
                local_ignored,
            )?;
            Ok::<(), crate::error::Error>(())
        };
        if root_files.len() >= 8 && threads.get() > 1 {
            let est_bytes = root_files.len().saturating_mul(512);
            let per_file = try_par_hash_with(&root_files, threads, est_bytes, |entry| {
                let mut local_untracked = Vec::new();
                let mut local_ignored = Vec::new();
                scan_root_file(entry, &mut local_untracked, &mut local_ignored)?;
                Ok((local_untracked, local_ignored))
            })
            .map_err(|e: ParallelHashError<crate::error::Error>| match e {
                ParallelHashError::Task(err) => err,
            })?;
            for (mut local_u, mut local_i) in per_file {
                untracked.append(&mut local_u);
                ignored.append(&mut local_i);
            }
        } else {
            let mut matcher = matcher_template.clone();
            for (name, entry) in &root_files {
                let _ = visit_untracked_dir_entry(
                    repo,
                    index,
                    work_tree,
                    gitlinks,
                    &mut matcher,
                    ignored_mode,
                    show_all,
                    false,
                    None::<&Cell<bool>>,
                    precompose_unicode,
                    tracked_paths,
                    "",
                    entry,
                    name,
                    pathspecs,
                    None,
                    &mut untracked,
                    &mut ignored,
                )?;
            }
        }
    }

    if root_dirs.is_empty() {
        return Ok((untracked, ignored));
    }

    let root: Arc<str> = Arc::from("");
    let mut tasks: Vec<UntrackedEntryTask> = root_dirs
        .into_iter()
        .map(|(name, entry)| UntrackedEntryTask {
            parent: Arc::clone(&root),
            name,
            entry,
        })
        .collect();
    let mut matcher_template = matcher_template.clone();
    if pathspecs.is_empty()
        && threads.get() > 1
        && index.entries.len() >= crate::worktree_scan::PARALLEL_STAT_MIN_ENTRIES
    {
        tasks = split_tracked_untracked_tasks(
            tasks,
            threads
                .get()
                .saturating_mul(UNTRACKED_SPLIT_DIRS_PER_THREAD),
            repo,
            index,
            gitlinks,
            precompose_unicode,
            tracked_paths,
            &mut matcher_template,
        );
    }

    let dir_count = tasks
        .iter()
        .filter(|t| crate::read_dir_kind::entry_is_dir(&t.entry).unwrap_or(false))
        .count();
    let options = crate::parallel::ParallelOptions {
        threads,
        min_items: crate::hash::PAR_HASH_MIN_ITEMS,
        min_total_bytes: Some(crate::hash::PAR_HASH_MIN_TOTAL_BYTES),
        signed_entry_threshold: None,
    };
    let per_task = crate::parallel::try_par_map_with_local(
        &tasks,
        options,
        dir_count.saturating_mul(4096),
        || matcher_template.clone(),
        |matcher, task| {
            // The matcher caches rules per directory it enters; starting each subtree from
            // the template keeps that cache bounded by one subtree instead of a worker's share.
            if crate::read_dir_kind::entry_is_dir(&task.entry).unwrap_or(false) {
                matcher.clone_from(&matcher_template);
            }
            let mut local_untracked = Vec::new();
            let mut local_ignored = Vec::new();
            let _ = visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                gitlinks,
                matcher,
                ignored_mode,
                show_all,
                false,
                None::<&Cell<bool>>,
                precompose_unicode,
                tracked_paths,
                &task.parent,
                &task.entry,
                &task.name,
                pathspecs,
                None,
                &mut local_untracked,
                &mut local_ignored,
            )?;
            Ok((local_untracked, local_ignored))
        },
    )
    .map_err(|e: ParallelHashError<crate::error::Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;

    for (mut local_u, mut local_i) in per_task {
        untracked.append(&mut local_u);
        ignored.append(&mut local_i);
    }
    Ok((untracked, ignored))
}

/// Directories per worker the parallel untracked walk aims for before it stops splitting.
const UNTRACKED_SPLIT_DIRS_PER_THREAD: usize = 8;

/// One directory entry queued for the parallel untracked walk.
struct UntrackedEntryTask {
    /// Stored repository-relative path of the containing directory (empty at the root).
    parent: Arc<str>,
    /// Raw file name as read from the directory.
    name: Vec<u8>,
    entry: DirEntry,
}

/// Replace directories whose walk is a plain recursion with their children until at least
/// `target_dirs` directories are queued (or nothing more can be split).
///
/// A directory qualifies when it holds tracked files and is neither tracked itself, a gitlink,
/// nor a nested submodule work tree: [`visit_untracked_directory`] then visits its children
/// in name order with no collapsing, so splitting it preserves the concatenated output. The
/// per-directory ignore rules of every split directory are loaded into `matcher`, which seeds
/// the workers.
#[allow(clippy::too_many_arguments)]
fn split_tracked_untracked_tasks(
    mut tasks: Vec<UntrackedEntryTask>,
    target_dirs: usize,
    repo: &Repository,
    index: &Index,
    gitlinks: &HashSet<String>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    matcher: &mut IgnoreMatcher,
) -> Vec<UntrackedEntryTask> {
    loop {
        let dirs = tasks
            .iter()
            .filter(|t| crate::read_dir_kind::entry_is_dir(&t.entry).unwrap_or(false))
            .count();
        if dirs >= target_dirs {
            return tasks;
        }
        let mut split_any = false;
        let mut next = Vec::with_capacity(tasks.len());
        for task in tasks {
            let Some(rel) = plain_tracked_directory(
                &task,
                repo,
                index,
                gitlinks,
                precompose_unicode,
                tracked_paths,
            ) else {
                next.push(task);
                continue;
            };
            split_any = true;
            let _ = matcher.note_directory_entered(repo, Some(index), &rel);
            let Ok(entries) = fs::read_dir(task.entry.path()) else {
                continue;
            };
            let parent: Arc<str> = Arc::from(rel);
            next.extend(
                named_entries_sorted(entries)
                    .into_iter()
                    .map(|(name, entry)| UntrackedEntryTask {
                        parent: Arc::clone(&parent),
                        name,
                        entry,
                    }),
            );
        }
        tasks = next;
        if !split_any {
            return tasks;
        }
    }
}

/// The stored path of `task` when it is a directory whose untracked walk only recurses into
/// its children (see [`split_tracked_untracked_tasks`]).
fn plain_tracked_directory(
    task: &UntrackedEntryTask,
    repo: &Repository,
    index: &Index,
    gitlinks: &HashSet<String>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
) -> Option<String> {
    if task.name == b".git" || !crate::read_dir_kind::entry_is_dir(&task.entry).unwrap_or(false) {
        return None;
    }
    let child_bytes = untracked_child_path(&task.parent, &task.name, precompose_unicode);
    if tracked_paths.contains_bytes(&child_bytes) {
        return None;
    }
    let rel = crate::repo_relpath::storage_string(&child_bytes);
    if gitlinks.contains(&rel)
        || !has_tracked_under(index, gitlinks, &rel)
        || dir_is_nested_submodule_worktree(repo.git_dir(), &task.entry.path())
    {
        return None;
    }
    Some(rel)
}

/// Raw repository-relative path of entry `name` inside the directory stored as `rel`,
/// precomposing the name when `core.precomposeUnicode` applies.
fn untracked_child_path(rel: &str, name: &[u8], precompose_unicode: bool) -> Vec<u8> {
    let precomposed = precompose_unicode
        .then(|| std::str::from_utf8(name).ok())
        .flatten()
        .and_then(|raw_name| match precompose_utf8_segment(raw_name) {
            Cow::Borrowed(_) => None,
            Cow::Owned(s) => Some(s.into_bytes()),
        });
    let name = precomposed.as_deref().unwrap_or(name);
    crate::repo_relpath::join_rel(&crate::repo_relpath::storage_bytes(rel), name)
}

#[allow(clippy::too_many_arguments)]
fn visit_untracked_dir_entry(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: IgnoredMode,
    show_all: bool,
    check_only: bool,
    visible_out: Option<&Cell<bool>>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel: &str,
    entry: &DirEntry,
    name: &[u8],
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<UntrackedWalkStep> {
    if name == b".git" {
        return Ok(UntrackedWalkStep::Continue);
    }
    let child_bytes = untracked_child_path(rel, name, precompose_unicode);
    let is_dir = crate::read_dir_kind::entry_is_dir(entry).unwrap_or(false);

    // Most entries in a typical work tree are tracked; settle those before building the
    // stored path string or the absolute path.
    if tracked_paths.contains_bytes(&child_bytes) {
        // A tracked non-directory is settled; a directory still needs walking unless the
        // index records it as a tree or gitlink.
        let skip = !is_dir
            || index
                .get(&child_bytes, 0)
                .is_some_and(|e| e.mode == MODE_TREE || e.mode == MODE_GITLINK);
        if skip {
            return Ok(UntrackedWalkStep::Continue);
        }
    }

    let child_rel = crate::repo_relpath::storage_string(&child_bytes);
    if is_dir && gitlinks.contains(&child_rel) {
        return Ok(UntrackedWalkStep::Continue);
    }
    let child_rel_match = crate::repo_relpath::as_utf8_for_matching(&child_rel);

    if is_dir {
        if !pathspec_may_match_directory(&child_rel, pathspecs) {
            return Ok(UntrackedWalkStep::Continue);
        }
        let path = entry.path();
        visit_untracked_directory(
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            check_only,
            visible_out,
            precompose_unicode,
            tracked_paths,
            &child_rel,
            &path,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        )?;
        if check_only && visible_out.is_some_and(|out| out.get()) {
            return Ok(UntrackedWalkStep::Stop);
        }
    } else {
        if crate::read_dir_kind::entry_is_git_ignored_special(entry).unwrap_or(true) {
            return Ok(UntrackedWalkStep::Continue);
        }
        if !status_path_matches_worktree_with_rules(
            repo, index, work_tree, &child_rel, pathspecs, rules,
        ) {
            return Ok(UntrackedWalkStep::Continue);
        }
        let is_ign = matcher.is_path_ignored(repo, Some(index), child_rel_match.as_ref(), false)?;
        if is_ign {
            if !check_only && ignored_mode != IgnoredMode::No {
                ignored_out.push(child_rel);
            }
        } else if let Some(out) = visible_out {
            out.set(true);
            return Ok(UntrackedWalkStep::Stop);
        } else {
            untracked_out.push(child_rel);
        }
    }

    Ok(UntrackedWalkStep::Continue)
}

#[allow(clippy::too_many_arguments)]
fn visit_untracked_read_dir_lazy(
    entries: impl Iterator<Item = std::io::Result<DirEntry>>,
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: IgnoredMode,
    show_all: bool,
    visible_out: Option<&Cell<bool>>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel: &str,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<()> {
    for entry in entries {
        if visible_out.is_some_and(|out| out.get()) {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        #[cfg(test)]
        untracked_walk_probe::record_entry();
        let name = dir_entry_name_bytes(&entry);
        if matches!(
            visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                gitlinks,
                matcher,
                ignored_mode,
                show_all,
                true,
                visible_out,
                precompose_unicode,
                tracked_paths,
                rel,
                &entry,
                &name,
                pathspecs,
                rules,
                untracked_out,
                ignored_out,
            )?,
            UntrackedWalkStep::Stop
        ) {
            return Ok(());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn visit_untracked_node(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: IgnoredMode,
    show_all: bool,
    check_only: bool,
    visible_out: Option<&Cell<bool>>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel: &str,
    abs: &Path,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<()> {
    if check_only && visible_out.is_some_and(|out| out.get()) {
        return Ok(());
    }

    let entries = match fs::read_dir(abs) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    let tracked_subdir = !rel.is_empty() && has_tracked_under(index, gitlinks, rel);
    if check_only && !tracked_subdir {
        return visit_untracked_read_dir_lazy(
            entries,
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            visible_out,
            precompose_unicode,
            tracked_paths,
            rel,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        );
    }

    let named = named_entries_sorted(entries);
    // The listing shows whether `.git` exists, so most directories skip the submodule probe.
    if tracked_subdir
        && named
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(b".git"))
        && dir_is_nested_submodule_worktree(repo.git_dir(), abs)
    {
        return Ok(());
    }

    if check_only {
        return visit_untracked_read_dir_lazy(
            named.into_iter().map(|(_, entry)| Ok(entry)),
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            visible_out,
            precompose_unicode,
            tracked_paths,
            rel,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        );
    }

    for (name, entry) in &named {
        if matches!(
            visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                gitlinks,
                matcher,
                ignored_mode,
                show_all,
                false,
                None,
                precompose_unicode,
                tracked_paths,
                rel,
                entry,
                name,
                pathspecs,
                rules,
                untracked_out,
                ignored_out,
            )?,
            UntrackedWalkStep::Stop
        ) {
            break;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn visit_untracked_directory(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: IgnoredMode,
    show_all: bool,
    check_only: bool,
    visible_out: Option<&Cell<bool>>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel: &str,
    abs: &Path,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<()> {
    let _ = matcher.note_directory_entered(repo, Some(index), rel);

    if has_tracked_under(index, gitlinks, rel) {
        visit_untracked_node(
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            check_only,
            visible_out,
            precompose_unicode,
            tracked_paths,
            rel,
            abs,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        )?;
        return Ok(());
    }

    // Fast prune: in default ignored mode, a directory excluded as a directory cannot contribute
    // visible untracked paths (and tracked descendants were handled above).
    if ignored_mode == IgnoredMode::No && matcher.is_path_ignored(repo, Some(index), rel, true)? {
        return Ok(());
    }

    if !rel.is_empty() && is_unregistered_embedded_git_repo(index, rel, abs) {
        untracked_out.push(format!("{rel}/"));
        return Ok(());
    }

    // Git `dir.c`: with `--ignored=matching` and full untracked listing, an excluded
    // directory is reported as a single path without enumerating children (unless
    // tracked files force a full walk — handled above).
    if ignored_mode == IgnoredMode::Matching
        && show_all
        && matcher.is_path_ignored(repo, Some(index), rel, true)?
    {
        ignored_out.push(format!("{rel}/"));
        return Ok(());
    }

    if ignored_mode != IgnoredMode::No
        && dir_is_nested_submodule_worktree(repo.git_dir(), abs)
        && matcher.is_path_ignored(repo, Some(index), rel, true)?
    {
        ignored_out.push(format!("{rel}/"));
        return Ok(());
    }

    if ignored_mode == IgnoredMode::Traditional
        && !show_all
        && directory_pathspec_matches_self(rel, pathspecs)
    {
        if let Some(dir_line) = traditional_normal_directory_only(
            repo, index, work_tree, gitlinks, matcher, rel, abs, pathspecs, rules,
        )? {
            ignored_out.push(dir_line);
            return Ok(());
        }
    }

    if check_only {
        visit_untracked_node(
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            true,
            visible_out,
            precompose_unicode,
            tracked_paths,
            rel,
            abs,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        )?;
        return Ok(());
    }

    // Git `dir.c` with `DIR_HIDE_EMPTY_DIRECTORIES`: probe for a visible untracked
    // entry without enumerating every child, then collapse to `dir/` in normal mode.
    if !show_all
        && ignored_mode == IgnoredMode::No
        && (pathspecs.is_empty() || directory_pathspec_matches_self(rel, pathspecs))
    {
        let found = Cell::new(false);
        visit_untracked_node(
            repo,
            index,
            work_tree,
            gitlinks,
            matcher,
            ignored_mode,
            false,
            true,
            Some(&found),
            precompose_unicode,
            tracked_paths,
            rel,
            abs,
            pathspecs,
            rules,
            untracked_out,
            ignored_out,
        )?;
        if found.get() {
            if !rel.is_empty() {
                untracked_out.push(format!("{rel}/"));
            }
            return Ok(());
        }
        if !rel.is_empty() && directory_contains_only_dot_git(abs) {
            untracked_out.push(format!("{rel}/"));
        }
        return Ok(());
    }

    let mut sub_untracked = Vec::new();
    let mut sub_ignored = Vec::new();
    visit_untracked_node(
        repo,
        index,
        work_tree,
        gitlinks,
        matcher,
        ignored_mode,
        show_all,
        false,
        None,
        precompose_unicode,
        tracked_paths,
        rel,
        abs,
        pathspecs,
        rules,
        &mut sub_untracked,
        &mut sub_ignored,
    )?;

    if show_all {
        untracked_out.append(&mut sub_untracked);
        ignored_out.append(&mut sub_ignored);
        return Ok(());
    }

    // `--untracked-files=normal`: collapse subtrees like Git's `walk_for_untracked`.
    if !sub_untracked.is_empty() && !sub_ignored.is_empty() {
        if !rel.is_empty() && directory_pathspec_matches_self(rel, pathspecs) {
            untracked_out.push(format!("{rel}/"));
        } else {
            untracked_out.append(&mut sub_untracked);
        }
        ignored_out.append(&mut sub_ignored);
        return Ok(());
    }

    if sub_untracked.is_empty() && !sub_ignored.is_empty() {
        let dir_excluded = matcher.is_path_ignored(repo, Some(index), rel, true)?;
        let collapse_matching = ignored_mode == IgnoredMode::Matching && dir_excluded;
        let collapse_traditional = ignored_mode == IgnoredMode::Traditional
            && directory_pathspec_matches_self(rel, pathspecs);
        if collapse_matching || collapse_traditional {
            ignored_out.push(format!("{rel}/"));
        } else {
            ignored_out.append(&mut sub_ignored);
        }
        return Ok(());
    }

    if !sub_untracked.is_empty() && sub_ignored.is_empty() {
        if rel.is_empty() || !directory_pathspec_matches_self(rel, pathspecs) {
            untracked_out.append(&mut sub_untracked);
        } else {
            untracked_out.push(format!("{rel}/"));
        }
        return Ok(());
    }

    // Match Git's normal untracked mode: keep directories that are empty apart from an internal
    // `.git` as collapsed `dir/` entries, but do not surface directories that only contain
    // ignored entries (t7063 expects those to stay hidden).
    if sub_untracked.is_empty()
        && sub_ignored.is_empty()
        && !rel.is_empty()
        && directory_contains_only_dot_git(abs)
    {
        untracked_out.push(format!("{rel}/"));
        return Ok(());
    }

    Ok(())
}

fn directory_contains_only_dot_git(dir: &Path) -> bool {
    let entries: Vec<_> = match fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return false,
    };
    !entries.is_empty()
        && entries
            .iter()
            .all(|e| e.file_name().to_string_lossy() == ".git")
}

/// Full tree scan: true when every file under `abs` is ignored and nothing untracked is present.
#[allow(clippy::too_many_arguments)]
fn traditional_normal_directory_only(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    gitlinks: &HashSet<String>,
    matcher: &mut IgnoreMatcher,
    rel: &str,
    abs: &Path,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
) -> Result<Option<String>> {
    let mut any_file = false;
    let mut stack = vec![abs.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let rel_child = crate::git_path::strip_worktree_prefix(&path, work_tree)
                .unwrap_or_else(|| name.clone());
            if !pathspec_may_match_directory(&rel_child, pathspecs)
                && !(entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                    && status_path_matches_worktree_with_rules(
                        repo, index, work_tree, &rel_child, pathspecs, rules,
                    ))
            {
                continue;
            }
            if index.get(rel_child.as_bytes(), 0).is_some() {
                return Ok(None);
            }
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir && gitlinks.contains(&rel_child) {
                continue;
            }
            if is_dir {
                stack.push(path);
            } else if !crate::read_dir_kind::entry_is_git_ignored_special(&entry).unwrap_or(true) {
                any_file = true;
                let ig = matcher.is_path_ignored(repo, Some(index), &rel_child, false)?;
                if !ig {
                    return Ok(None);
                }
            }
        }
    }

    let dir_ignored = matcher.is_path_ignored(repo, Some(index), rel, true)?;
    if !any_file {
        return Ok(if dir_ignored {
            Some(format!("{rel}/"))
        } else {
            None
        });
    }

    Ok(Some(format!("{rel}/")))
}

fn has_tracked_under(index: &Index, gitlinks: &HashSet<String>, rel_dir: &str) -> bool {
    if index.has_stage0_descendant_under(rel_dir) {
        return true;
    }
    gitlinks.iter().any(|g| {
        g.as_str() == rel_dir || (!rel_dir.is_empty() && g.starts_with(&format!("{rel_dir}/")))
    })
}

/// Whether `dir` is the work tree of a nested submodule of the superproject at
/// `super_git_dir` (its `.git` resolves under `super_git_dir/modules`).
pub fn dir_is_nested_submodule_worktree(super_git_dir: &Path, dir: &Path) -> bool {
    let gitfile = dir.join(".git");
    if gitfile.is_dir() {
        return true;
    }
    let Ok(content) = fs::read_to_string(&gitfile) else {
        return false;
    };
    let Some(rest) = content.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return false;
    };
    let raw = rest.trim();
    if raw.is_empty() {
        return false;
    }
    let gitdir_path = Path::new(raw);
    let resolved = if gitdir_path.is_absolute() {
        gitdir_path.to_path_buf()
    } else {
        dir.join(gitdir_path)
    };
    let Ok(resolved_canon) = resolved.canonicalize() else {
        return false;
    };
    let modules_root = super_git_dir.join("modules");
    let Ok(modules_canon) = modules_root.canonicalize() else {
        return false;
    };
    resolved_canon.starts_with(&modules_canon)
}

/// Pathspec match for status using git's exclude / OR-of-positives semantics.
pub fn status_path_matches(path: &str, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let normalized = path.trim_end_matches('/');
    let excluded = pathspecs.iter().any(|spec| {
        crate::pathspec::pathspec_exclude_matches(spec, path)
            || crate::pathspec::pathspec_exclude_matches(spec, normalized)
    });
    if excluded {
        return false;
    }
    let mut has_positive = false;
    let mut positive_match = false;
    for spec in pathspecs {
        if crate::pathspec::pathspec_is_exclude(spec) {
            continue;
        }
        has_positive = true;
        if crate::pathspec::pathspec_matches(spec, path)
            || crate::pathspec::pathspec_matches(spec, normalized)
        {
            positive_match = true;
        }
    }
    !has_positive || positive_match
}

fn pathspecs_use_attr_magic(pathspecs: &[String]) -> bool {
    pathspecs
        .iter()
        .any(|spec| spec.starts_with(":(attr:") || spec.contains(",attr:"))
}

/// Pathspec match that also honors `:(attr:...)` magic against worktree contents.
pub fn status_path_matches_worktree(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    path: &str,
    pathspecs: &[String],
) -> bool {
    status_path_matches_worktree_with_rules(repo, index, work_tree, path, pathspecs, None)
}

/// Like [`status_path_matches_worktree`], reusing an existing [`crate::worktree_rules::WorktreeRules`] when provided.
pub fn status_path_matches_worktree_with_rules(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    path: &str,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    if !pathspecs_use_attr_magic(pathspecs) {
        return status_path_matches(path, pathspecs);
    }

    let normalized = path.trim_end_matches('/');
    let attrs = if let Some(ctx) = rules {
        ctx.attribute_rules_for_path(normalized)
    } else {
        crate::worktree_rules::WorktreeRules::from_repository(repo, index)
            .ok()
            .map(|ctx| ctx.attribute_rules_for_path(normalized))
            .unwrap_or_else(|| {
                crate::crlf::load_gitattributes_for_checkout(
                    work_tree,
                    normalized,
                    index,
                    repo.odb(),
                )
            })
    };
    let mode = worktree_path_mode(&work_tree.join(normalized));
    crate::pathspec::matches_pathspec_list_for_object(normalized, mode, &attrs, pathspecs)
}

fn worktree_path_mode(path: &Path) -> u32 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0o120000;
    }
    if meta.is_dir() {
        return MODE_TREE;
    }
    if is_executable_file(&meta) {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(unix)]
fn is_executable_file(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(_meta: &fs::Metadata) -> bool {
    false
}

/// Whether `rel_dir` could contain a path matching any of `pathspecs` (directory prune).
pub fn pathspec_may_match_directory(rel_dir: &str, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    if pathspecs_use_attr_magic(pathspecs) {
        return true;
    }
    let rel_dir = rel_dir.trim_end_matches('/');
    if rel_dir.is_empty() {
        return true;
    }
    pathspecs.iter().any(|spec| {
        if crate::pathspec::has_glob_chars(spec) {
            return true;
        }
        let spec_norm = spec.trim_end_matches('/');
        spec_norm == rel_dir
            || spec_norm.starts_with(&format!("{rel_dir}/"))
            || rel_dir.starts_with(&format!("{spec_norm}/"))
            || crate::pathspec::pathspec_matches(spec, rel_dir)
    })
}

fn directory_pathspec_matches_self(rel_dir: &str, pathspecs: &[String]) -> bool {
    pathspecs.is_empty()
        || status_path_matches(&format!("{}/", rel_dir.trim_end_matches('/')), pathspecs)
}

use crate::diff::DiffStatus;

/// Collect unmerged paths and their [`ConflictKind`] from the index.
#[must_use]
pub fn conflicts_from_index(index: &Index) -> Vec<Conflict> {
    let paths = conflict_paths_from_index_raw(index);
    paths
        .into_iter()
        .map(|path_bytes| {
            let stage1 = index.get(&path_bytes, 1).is_some();
            let stage2 = index.get(&path_bytes, 2).is_some();
            let stage3 = index.get(&path_bytes, 3).is_some();
            Conflict {
                path: index_relpath_to_string(&path_bytes),
                kind: conflict_kind_from_stages(stage1, stage2, stage3),
            }
        })
        .collect()
}

fn index_relpath_to_string(path: &[u8]) -> String {
    crate::repo_relpath::storage_string(path)
}

#[cfg(unix)]
fn dir_entry_name_bytes(entry: &std::fs::DirEntry) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    entry.file_name().into_vec()
}

#[cfg(not(unix))]
fn dir_entry_name_bytes(entry: &std::fs::DirEntry) -> Vec<u8> {
    entry
        .file_name()
        .to_string_lossy()
        .into_owned()
        .into_bytes()
}

/// Collect readable entries of `entries` with their raw file names, ordered by name bytes.
///
/// Extracting each name once keeps sorting allocation-free; `DirEntry::file_name` allocates
/// on every call. Names within one directory are unique, so an unstable sort is exact.
fn named_entries_sorted(entries: ReadDir) -> Vec<(Vec<u8>, DirEntry)> {
    let mut named: Vec<(Vec<u8>, DirEntry)> = entries
        .filter_map(|e| e.ok())
        .map(|e| (dir_entry_name_bytes(&e), e))
        .collect();
    named.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    named
}

fn conflict_paths_from_index_raw(index: &Index) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        let stage = entry.stage();
        if (1_u8..=3).contains(&stage) {
            paths.insert(entry.path.clone());
        }
    }
    paths.into_iter().collect()
}

/// Deduplicate conflict paths across staged/unstaged lists for status display.
///
/// Each conflict path appears once under staged as [`DiffStatus::Unmerged`]; redundant
/// unstaged rows (duplicate unmerged or worktree modified) are dropped.
pub fn normalize_status_conflicts(
    staged: &mut Vec<DiffEntry>,
    unstaged: &mut Vec<DiffEntry>,
    conflicts: &[String],
) {
    if conflicts.is_empty() {
        return;
    }
    let conflict_set: BTreeSet<&str> = conflicts.iter().map(String::as_str).collect();
    unstaged.retain(|e| !conflict_set.contains(e.path()));
    let mut seen_staged = BTreeSet::new();
    staged.retain(|e| {
        if !conflict_set.contains(e.path()) {
            return true;
        }
        if e.status != DiffStatus::Unmerged {
            return true;
        }
        seen_staged.insert(e.path().to_owned())
    });
}

// --- The status operation ---------------------------------------------------

use crate::progress::ProgressSink;

/// Compute the status of `repo`'s work tree as a [`StatusModel`].
///
/// This is the clean library computation: load and sparse-expand the index,
/// resolve HEAD and the in-progress operation [`state`](crate::state), compute
/// the staged (index-vs-HEAD) and unstaged (index-vs-worktree) diffs with
/// optional rename detection, walk the work tree for untracked/ignored paths,
/// and count stash entries.
///
/// The `grit` CLI's performance and diagnostic layers — the fsmonitor query, the
/// untracked cache, and trace2 emission — are intentionally *not* part of this;
/// they wrap the call in the binary. A library consumer that just wants the
/// status of a repository calls this directly.
pub fn status(
    repo: &Repository,
    opts: &StatusOptions,
    _progress: &mut dyn ProgressSink,
) -> Result<StatusModel> {
    crate::trace::trace_span!("grit.status.scan");
    let work_tree = repo
        .work_tree()
        .ok_or(crate::error::Error::RequiresWorkTree)?;

    let head = crate::state::resolve_head(repo.git_dir())?;
    let state = crate::state::wt_status_get_state(repo.git_dir(), &head, true)?;
    let repo_config = repo.config().ok();

    // Load the index (including split-index merge and GIT_INDEX_FILE), remembering
    // whether it was sparse on disk, then expand sparse-directory placeholders
    // so the diffs see real entries.
    let index_path = repo.index_path_for_env()?;
    let mut index = repo.load_index_at(&index_path)?;
    let index_mtime = index.source_mtime;
    let worktree_rules = Arc::new(Mutex::new(
        crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?,
    ));
    let sparse_directory_prefixes: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.is_sparse_directory_placeholder())
        .map(|e| e.path.clone())
        .collect();
    let index_sparse_on_disk =
        index.sparse_directories || index.has_sparse_directory_placeholders();
    let protect = repo_config
        .as_deref()
        .map(crate::path_validate::ProtectOptions::from_config)
        .unwrap_or_default();
    let _ = index.expand_sparse_directory_placeholders(repo.odb(), protect);

    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb().read(oid)?;
            Some(crate::objects::parse_commit(&obj.data)?.tree)
        }
        None => None,
    };

    // Staged: index vs HEAD tree, narrowed to pathspecs before rename detection.
    let mut staged: Vec<DiffEntry> = if opts.pathspecs.is_empty()
        && index.entries.iter().all(|e| e.stage() == 0)
        && head_tree
            .as_ref()
            .is_some_and(|t| index.cache_tree_root.as_ref() == Some(t))
        && crate::write_tree::cache_tree_extension_valid(index.cache_tree.as_ref())
    {
        Vec::new()
    } else {
        crate::diff::diff_index_to_tree(repo.odb(), &index, head_tree.as_ref(), false)?
            .into_iter()
            .filter(|e| status_diff_entry_matches_pathspec(e, &opts.pathspecs))
            .collect()
    };

    // Unstaged: worktree vs index, narrowed before rename detection.
    let diff_index_mtime = crate::diff::index_mtime_for_diff(&index, index_mtime);
    let (unstaged_raw, index_stat_refresh_changed) =
        crate::diff::diff_index_to_worktree_with_options(
            repo.odb(),
            &mut index,
            work_tree,
            crate::diff::DiffIndexToWorktreeOptions {
                index_mtime: diff_index_mtime,
                ignore_submodule_untracked: opts.untracked == UntrackedMode::No,
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                refresh_index_stat_in_pass: true,
                config: repo_config.clone(),
                stat_parallel_threads: opts.stat_parallel_threads,
                thread_policy: Some(repo.thread_policy()),
                worktree_rules: Some(Arc::clone(&worktree_rules)),
                for_status: true,
                command_runner: Some(repo.command_runner()),
                fsmonitor_query_boundary: opts.fsmonitor_query_boundary,
                ..Default::default()
            },
        )?;
    let mut persist_index = index_stat_refresh_changed;
    let mut unstaged: Vec<DiffEntry> = unstaged_raw
        .into_iter()
        .filter(|e| status_diff_entry_matches_pathspec(e, &opts.pathspecs))
        .collect();

    if let Some(rd) = opts.renames {
        let thread_policy = repo.thread_policy();
        let rename_config = repo
            .config()
            .map(|c| config_for_status_rename_detection(c.as_ref()))
            .unwrap_or_default();
        staged = apply_status_renames(
            repo.odb(),
            staged,
            rd,
            head_tree.as_ref(),
            &rename_config,
            &thread_policy,
        )?;
        unstaged = apply_status_renames(
            repo.odb(),
            unstaged,
            rd,
            head_tree.as_ref(),
            &rename_config,
            &thread_policy,
        )?;
    }

    let mut conflict_details = conflicts_from_index(&index);
    if !opts.pathspecs.is_empty() {
        conflict_details.retain(|c| status_path_matches(&c.path, &opts.pathspecs));
    }
    let conflicts: Vec<String> = conflict_details.iter().map(|c| c.path.clone()).collect();
    normalize_status_conflicts(&mut staged, &mut unstaged, &conflicts);

    let (untracked, ignored) =
        if opts.untracked == UntrackedMode::No && opts.ignored == IgnoredMode::No {
            (Vec::new(), Vec::new())
        } else {
            let rules_guard = worktree_rules.lock().map_err(|e| {
                crate::error::Error::Message(format!("worktree rules lock poisoned: {e}"))
            })?;
            let (mut untracked, ignored, collect_meta) = collect_untracked_and_ignored_with_rules(
                repo,
                &mut index,
                work_tree,
                opts.ignored,
                opts.untracked == UntrackedMode::All,
                &opts.pathspecs,
                &rules_guard,
            )?;
            persist_index |= collect_meta.untracked_cache_refreshed;
            if opts.untracked == UntrackedMode::No {
                untracked.clear();
            }
            (untracked, ignored)
        };

    if persist_index && repo.try_write_index_at(&index_path, &mut index)? {
        index.source_mtime = crate::index::index_file_mtime(&index_path);
    }

    let stash_count = if opts.count_stash {
        crate::reflog::reflog_entry_count(repo.git_dir(), "refs/stash").unwrap_or(0)
    } else {
        0
    };

    let operation = status_operation(&state, !conflicts.is_empty());

    Ok(StatusModel {
        head,
        head_tree,
        state,
        index,
        staged,
        unstaged,
        untracked,
        ignored,
        conflicts,
        conflict_details,
        operation,
        stash_count,
        index_sparse_on_disk,
        sparse_directory_prefixes,
    })
}

/// Apply status rename (and optionally copy) detection, mirroring git's
/// candidate-count guards so a huge add/delete set is left undetected.
fn apply_status_renames(
    odb: &crate::odb::Odb,
    entries: Vec<DiffEntry>,
    rd: RenameDetection,
    head_tree: Option<&ObjectId>,
    config: &crate::config::ConfigSet,
    thread_policy: &crate::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    use crate::diff::DiffStatus;
    const MATRIX_BUDGET: usize = 50_000;
    const CANDIDATE_LIMIT: usize = 2_000;

    let mut deleted = 0usize;
    let mut added = 0usize;
    for entry in &entries {
        match entry.status {
            DiffStatus::Deleted => deleted += 1,
            DiffStatus::Added => added += 1,
            _ => {}
        }
    }
    if deleted == 0 || added == 0 {
        return Ok(entries);
    }
    if deleted.saturating_add(added) > CANDIDATE_LIMIT
        || deleted.saturating_mul(added) > MATRIX_BUDGET
    {
        return Ok(entries);
    }
    if rd.copies {
        return crate::diff::status_apply_rename_copy_detection(
            odb,
            entries,
            rd.threshold,
            true,
            head_tree,
            config,
            thread_policy,
        );
    }
    Ok(crate::diff::detect_renames_with_policy(
        odb,
        None,
        entries,
        rd.threshold,
        config,
        thread_policy,
    ))
}

/// Render [`StatusModel`] as `git status --porcelain=v2` lines (no `#` headers).
///
/// Covers ordinary tracked changes, untracked paths, and unmerged entries for
/// non-submodule repositories. Submodule and rename/copy extended lines are omitted
/// when rename detection is disabled (the default [`StatusOptions`]).
pub fn format_porcelain_v2(
    model: &StatusModel,
    odb: &crate::odb::Odb,
    work_tree: Option<&std::path::Path>,
) -> Vec<String> {
    use crate::diff::{format_mode, head_path_states};
    use std::collections::BTreeMap;

    let null_oid_hex = ObjectId::null(odb.hash_algo()).to_hex();
    let head_states = head_path_states(odb, model.head_tree.as_ref()).unwrap_or_default();

    let mut staged_by_path: BTreeMap<String, &DiffEntry> = BTreeMap::new();
    for e in &model.staged {
        staged_by_path.insert(e.path().to_owned(), e);
    }
    let mut unstaged_by_path: BTreeMap<String, &DiffEntry> = BTreeMap::new();
    for e in &model.unstaged {
        unstaged_by_path.insert(e.path().to_owned(), e);
    }

    let conflict_set: BTreeMap<&str, ()> = model
        .conflicts
        .iter()
        .map(String::as_str)
        .map(|p| (p, ()))
        .collect();

    let mut paths: BTreeMap<String, ()> = BTreeMap::new();
    for p in staged_by_path.keys().chain(unstaged_by_path.keys()) {
        paths.insert(p.clone(), ());
    }

    let mut out = Vec::new();

    for path in paths.keys() {
        if conflict_set.contains_key(path.as_str()) {
            continue;
        }
        let x = staged_by_path
            .get(path.as_str())
            .map(|e| e.status.letter())
            .unwrap_or('.');
        let y = unstaged_by_path
            .get(path.as_str())
            .map(|e| e.status.letter())
            .unwrap_or('.');
        if x == '.' && y == '.' {
            continue;
        }

        let (m_h, h_h) = head_states
            .get(path.as_str())
            .map(|(m, oid)| (format_mode(*m), oid.to_hex()))
            .unwrap_or_else(|| ("000000".to_owned(), null_oid_hex.clone()));

        let idx_e = model
            .index
            .entries()
            .iter()
            .find(|e| e.stage() == 0 && String::from_utf8_lossy(&e.path) == *path);
        let (m_i, h_i) = idx_e
            .map(|e| {
                if e.intent_to_add() {
                    ("000000".to_owned(), null_oid_hex.clone())
                } else {
                    (format_mode(e.mode), e.oid.to_hex())
                }
            })
            .unwrap_or_else(|| ("000000".to_owned(), null_oid_hex.clone()));

        let m_w = porcelain_v2_worktree_mode(
            x,
            y,
            unstaged_by_path.get(path.as_str()).copied(),
            work_tree,
            path.as_str(),
        );

        out.push(format!(
            "1 {}{} N... {m_h} {m_i} {m_w} {h_h} {h_i} {path}",
            x, y
        ));
    }

    for path in &model.untracked {
        out.push(format!("? {path}"));
    }

    for path in &model.conflicts {
        let stages: Vec<_> = model
            .index
            .entries()
            .iter()
            .filter(|e| String::from_utf8_lossy(&e.path) == *path && e.stage() != 0)
            .collect();
        if stages.is_empty() {
            continue;
        }
        let stagemask = unmerged_stagemask(&stages);
        let xy = porcelain_v2_unmerged_xy(stagemask);
        let m_w = porcelain_v2_worktree_mode('.', '.', None, work_tree, path);
        let mut fields = vec![
            "u".to_owned(),
            format!("{}{}", xy.0, xy.1),
            "N...".to_owned(),
        ];
        for stage in 1u8..=3 {
            let mode = stages
                .iter()
                .find(|e| e.stage() == stage)
                .map(|e| format_mode(e.mode))
                .unwrap_or_else(|| "000000".to_owned());
            fields.push(mode);
        }
        fields.push(m_w);
        for stage in 1u8..=3 {
            let oid = stages
                .iter()
                .find(|e| e.stage() == stage)
                .map(|e| e.oid.to_hex())
                .unwrap_or_else(|| null_oid_hex.clone());
            fields.push(oid);
        }
        fields.push(path.clone());
        out.push(fields.join(" "));
    }

    out.sort();
    out
}

/// Path half of a [`StatusShortLine`] (single path or rename/copy endpoints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusShortPath {
    /// One repository-relative path.
    Single(String),
    /// Rename or copy with distinct old and new paths.
    Rename {
        /// Previous path.
        old: String,
        /// Current path.
        new: String,
    },
}

impl StatusShortPath {
    /// Render this path with a formatter applied to each repository-relative component.
    #[must_use]
    pub fn render_with<F>(&self, mut format_path: F) -> String
    where
        F: FnMut(&str) -> String,
    {
        match self {
            Self::Single(path) => format_path(path),
            Self::Rename { old, new } => {
                format!("{} -> {}", format_path(old), format_path(new))
            }
        }
    }
}

/// One line of compact status output before terminal path quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusShortLine {
    /// Ordinary tracked change (`XY path`).
    Tracked {
        /// Index (staged) status letter, or `.`.
        staged: char,
        /// Worktree (unstaged) status letter, or `.`.
        unstaged: char,
        /// Path or rename endpoints.
        path: StatusShortPath,
    },
    /// Untracked path (`?? path`).
    Untracked(String),
    /// Ignored path (`!! path`).
    Ignored(String),
    /// Unmerged path (`XY path` with conflict letters).
    Unmerged {
        /// Index column letter.
        staged: char,
        /// Worktree column letter.
        unstaged: char,
        /// Conflict path.
        path: String,
    },
}

impl StatusShortLine {
    /// Render this line, applying `format_path` to each repository-relative path segment.
    #[must_use]
    pub fn render_with<F>(&self, format_path: F) -> String
    where
        F: FnMut(&str) -> String,
    {
        match self {
            Self::Tracked {
                staged,
                unstaged,
                path,
            } => format!("{staged}{unstaged} {}", path.render_with(format_path)),
            Self::Untracked(path) => {
                let mut format_path = format_path;
                format!("?? {}", format_path(path))
            }
            Self::Ignored(path) => {
                let mut format_path = format_path;
                format!("!! {}", format_path(path))
            }
            Self::Unmerged {
                staged,
                unstaged,
                path,
            } => {
                let mut format_path = format_path;
                format!("{staged}{unstaged} {}", format_path(path))
            }
        }
    }
}

/// Collect compact status lines from `model` with structured paths (for `--short` rendering).
#[must_use]
pub fn short_status_lines(model: &StatusModel) -> Vec<StatusShortLine> {
    use std::collections::BTreeMap;

    let mut staged_by_path: BTreeMap<String, &DiffEntry> = BTreeMap::new();
    for e in &model.staged {
        staged_by_path.insert(e.path().to_owned(), e);
    }
    let mut unstaged_by_path: BTreeMap<String, &DiffEntry> = BTreeMap::new();
    for e in &model.unstaged {
        unstaged_by_path.insert(e.path().to_owned(), e);
    }

    let conflict_set: BTreeMap<&str, ()> = model
        .conflicts
        .iter()
        .map(String::as_str)
        .map(|p| (p, ()))
        .collect();

    let mut paths: BTreeMap<String, ()> = BTreeMap::new();
    for p in staged_by_path.keys().chain(unstaged_by_path.keys()) {
        paths.insert(p.clone(), ());
    }

    let mut out = Vec::new();

    for path in paths.keys() {
        if conflict_set.contains_key(path.as_str()) {
            continue;
        }
        let staged = staged_by_path.get(path.as_str());
        let unstaged = unstaged_by_path.get(path.as_str());
        let x = staged.map(|e| e.status.letter()).unwrap_or('.');
        let y = unstaged.map(|e| e.status.letter()).unwrap_or('.');
        if x == '.' && y == '.' {
            continue;
        }
        out.push(StatusShortLine::Tracked {
            staged: x,
            unstaged: y,
            path: short_status_path(staged.copied(), unstaged.copied(), path),
        });
    }

    for path in &model.untracked {
        out.push(StatusShortLine::Untracked(path.clone()));
    }

    for path in &model.ignored {
        out.push(StatusShortLine::Ignored(path.clone()));
    }

    for path in &model.conflicts {
        let stages: Vec<_> = model
            .index
            .entries()
            .iter()
            .filter(|e| String::from_utf8_lossy(&e.path) == *path && e.stage() != 0)
            .collect();
        if stages.is_empty() {
            continue;
        }
        let stagemask = unmerged_stagemask(&stages);
        let xy = porcelain_v2_unmerged_xy(stagemask);
        out.push(StatusShortLine::Unmerged {
            staged: xy.0,
            unstaged: xy.1,
            path: path.clone(),
        });
    }

    out.sort_by_key(short_status_line_sort_key);
    out
}

fn short_status_line_sort_key(line: &StatusShortLine) -> String {
    line.render_with(|p| p.to_owned())
}

/// Render [`StatusModel`] as compact two-column status lines (repository-relative paths).
///
/// Prefer [`short_status_lines`] when paths need cwd-relative display or quoting.
#[must_use]
pub fn format_short(model: &StatusModel) -> Vec<String> {
    short_status_lines(model)
        .into_iter()
        .map(|line| line.render_with(|p| p.to_owned()))
        .collect()
}

fn short_status_path(
    staged: Option<&DiffEntry>,
    unstaged: Option<&DiffEntry>,
    fallback: &str,
) -> StatusShortPath {
    use crate::diff::DiffStatus;
    let entry = staged.or(unstaged);
    let Some(entry) = entry else {
        return StatusShortPath::Single(fallback.to_owned());
    };
    if matches!(entry.status, DiffStatus::Renamed | DiffStatus::Copied) {
        if let Some(old) = entry.old_path.as_deref() {
            let new = entry.new_path.as_deref().unwrap_or(fallback);
            if old != new {
                return StatusShortPath::Rename {
                    old: old.to_owned(),
                    new: new.to_owned(),
                };
            }
        }
    }
    StatusShortPath::Single(entry.path().to_owned())
}

fn unmerged_stagemask(stages: &[&crate::index::IndexEntry]) -> u8 {
    let mut mask = 0u8;
    for e in stages {
        let stage = e.stage();
        if (1..=3).contains(&stage) {
            mask |= 1 << (stage - 1);
        }
    }
    mask
}

/// Map Git's unmerged stage bitmask to porcelain short status letters (see upstream `wt-status.c`).
fn porcelain_v2_unmerged_xy(stagemask: u8) -> (char, char) {
    let key = match stagemask {
        1 => "DD",
        2 => "AU",
        3 => "UD",
        4 => "UA",
        5 => "DU",
        6 => "AA",
        7 => "UU",
        other => {
            debug_assert!(false, "unexpected unmerged stagemask {other:#x}");
            "UU"
        }
    };
    let bytes = key.as_bytes();
    (bytes[0] as char, bytes[1] as char)
}

fn porcelain_v2_worktree_mode(
    staged_x: char,
    unstaged_y: char,
    unstaged: Option<&DiffEntry>,
    work_tree: Option<&std::path::Path>,
    path: &str,
) -> String {
    use crate::diff::{format_mode, mode_from_metadata, DiffStatus};

    if unstaged_y == '.' && staged_x == 'D' {
        return "000000".to_owned();
    }
    if let Some(entry) = unstaged {
        if entry.status == DiffStatus::Deleted {
            return "000000".to_owned();
        }
        if !entry.new_mode.is_empty() {
            return entry.new_mode.clone();
        }
    }
    if let Some(wt) = work_tree {
        let file_path = wt.join(path);
        match std::fs::symlink_metadata(&file_path) {
            Ok(meta) => return format_mode(mode_from_metadata(&meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                return "000000".to_owned();
            }
            Err(_) => {}
        }
    }
    if unstaged_y != '.' {
        return "000000".to_owned();
    }
    "000000".to_owned()
}

#[cfg(test)]
mod status_op_tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::progress::NullProgress;
    use std::fs;
    use tempfile::TempDir;

    fn init_min_repo(root: &std::path::Path) {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
    }

    #[test]
    fn untracked_mode_from_config_reads_show_untracked_files() {
        assert_eq!(
            untracked_mode_from_config(&ConfigSet::new()).unwrap(),
            UntrackedMode::Normal,
            "missing key defaults to normal"
        );
        let cfg_dir = tempfile::TempDir::new().unwrap();
        fs::write(
            cfg_dir.path().join("config"),
            "[status]\n\tshowUntrackedFiles = all\n",
        )
        .unwrap();
        let cfg_all = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(cfg_dir.path()),
            true,
        )
        .unwrap();
        assert_eq!(
            untracked_mode_from_config(&cfg_all).unwrap(),
            UntrackedMode::All
        );

        let cfg_dir = tempfile::TempDir::new().unwrap();
        fs::write(
            cfg_dir.path().join("config"),
            "[status]\n\tshowUntrackedFiles = no\n",
        )
        .unwrap();
        let cfg_no = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(cfg_dir.path()),
            true,
        )
        .unwrap();
        assert_eq!(
            untracked_mode_from_config(&cfg_no).unwrap(),
            UntrackedMode::No
        );

        let cfg_dir = tempfile::TempDir::new().unwrap();
        fs::write(
            cfg_dir.path().join("config"),
            "[status]\n\tshowUntrackedFiles = false\n",
        )
        .unwrap();
        let cfg_invalid = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(cfg_dir.path()),
            true,
        )
        .unwrap();
        assert_eq!(
            untracked_mode_from_config(&cfg_invalid).unwrap(),
            UntrackedMode::No
        );

        let cfg_dir = tempfile::TempDir::new().unwrap();
        fs::write(
            cfg_dir.path().join("config"),
            "[status]\n\tshowUntrackedFiles = true\n",
        )
        .unwrap();
        let cfg_true = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(cfg_dir.path()),
            true,
        )
        .unwrap();
        assert_eq!(
            untracked_mode_from_config(&cfg_true).unwrap(),
            UntrackedMode::Normal
        );
    }

    #[test]
    fn untracked_mode_from_config_accepts_git_boolean_synonyms() {
        for v in ["false", "off", "0", "OFF", "False"] {
            assert_eq!(
                untracked_mode_from_config_value(v),
                Some(UntrackedMode::No),
                "no synonym {v:?}"
            );
        }
        for v in ["true", "on", "yes", "1", "ON", "YES"] {
            assert_eq!(
                untracked_mode_from_config_value(v),
                Some(UntrackedMode::Normal),
                "normal synonym {v:?}"
            );
        }
        for v in ["2", "-1", "42", "01", "2k", "0x2"] {
            assert_eq!(
                untracked_mode_from_config_value(v),
                Some(UntrackedMode::Normal),
                "nonzero integer {v:?}"
            );
        }
        for v in ["08", "2147483648", "-2147483649", "1.5", "bogus", "2 "] {
            assert_eq!(untracked_mode_from_config_value(v), None, "rejected {v:?}");
        }
        assert_eq!(
            untracked_mode_from_config_value(" 2"),
            Some(UntrackedMode::Normal),
            "leading C-locale whitespace before integer"
        );
    }

    #[test]
    fn status_detects_untracked_file_on_unborn_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join("foo.txt"), b"hello\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();

        assert!(model.head_tree.is_none(), "unborn HEAD has no tree");
        assert!(
            model.staged.is_empty(),
            "nothing staged: {:?}",
            model.staged
        );
        assert!(
            model.unstaged.is_empty(),
            "nothing unstaged: {:?}",
            model.unstaged
        );
        assert!(
            model.untracked.iter().any(|p| p == "foo.txt"),
            "foo.txt should be untracked, got {:?}",
            model.untracked
        );
    }

    #[test]
    fn status_collapses_large_untracked_directory_without_full_walk() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        let scratch = root.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        for i in 0..3000 {
            fs::write(scratch.join(format!("u{i}.rs")), format!("{i}\n")).unwrap();
        }

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        untracked_walk_probe::reset();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();

        assert_eq!(
            model.untracked,
            vec!["scratch/".to_owned()],
            "normal untracked mode should collapse wholly untracked directories"
        );
        let entries_read = untracked_walk_probe::count();
        assert!(
            entries_read <= 8,
            "check-only probe must stop at the first visible entry, not scan all \
             3000 files (read {entries_read} directory entries during probe)"
        );
    }

    #[test]
    fn status_reports_hundred_modified_on_dense_leaf_fixture() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        const DIRS: usize = 40;
        const FILES_PER_DIR: usize = 50;
        for d in 0..DIRS {
            let dir = format!("p/a/b/leaf{d:04}");
            fs::create_dir_all(root.join(&dir)).unwrap();
            for f in 0..FILES_PER_DIR {
                fs::write(
                    root.join(&dir).join(format!("f{f}-file.txt")),
                    format!("body {d}/{f}\n"),
                )
                .unwrap();
            }
        }
        grit_test_support::git(root, &["add", "."]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let mut paths: Vec<_> = (0..DIRS)
            .flat_map(|d| {
                (0..FILES_PER_DIR)
                    .map(move |f| root.join(format!("p/a/b/leaf{d:04}/f{f}-file.txt")))
            })
            .collect();
        paths.sort();
        for path in paths.iter().take(100) {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(file, "dirty").unwrap();
        }

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let modified = model
            .unstaged
            .iter()
            .filter(|d| d.status == crate::diff::DiffStatus::Modified)
            .count();
        assert_eq!(
            modified, 100,
            "expected 100 modified paths, got unstaged {:?}",
            model.unstaged
        );
    }

    #[test]
    fn status_scan_stats_each_tracked_path_once() {
        use crate::worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        const FILE_COUNT: usize = 2000;
        for i in 0..FILE_COUNT {
            let dir = format!("d{:04}", i / 100);
            fs::create_dir_all(root.join(&dir)).unwrap();
            fs::write(
                root.join(&dir).join(format!("f{i:05}.txt")),
                format!("body {i}\n"),
            )
            .unwrap();
        }
        grit_test_support::git(root, &["add", "."]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let _probe = reset_worktree_metadata_probe(root);
        let _ = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let tracked = repo.load_index().unwrap().entries.len();
        let dir_count = (FILE_COUNT + 99) / 100;
        let calls = worktree_metadata_probe_count();
        assert!(
            calls <= tracked + dir_count + 5,
            "status scan metadata calls {calls} exceeded budget {} (tracked {tracked}, dirs {dir_count})",
            tracked + dir_count + 5
        );
    }

    #[test]
    fn status_lists_untracked_sibling_on_deep_indexed_chain() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/tracked.txt"), b"tracked\n").unwrap();
        grit_test_support::git(root, &["add", "a/b/c/tracked.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        fs::write(root.join("a/b/untracked.txt"), b"new\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        assert!(
            model.untracked.iter().any(|p| p == "a/b/untracked.txt"),
            "interior untracked sibling must be visible, got {:?}",
            model.untracked
        );
    }

    #[test]
    fn status_untracked_no_still_collects_ignored_when_requested() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("noise.log"), b"x\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            untracked: UntrackedMode::No,
            ignored: IgnoredMode::Traditional,
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        assert!(
            model.untracked.is_empty(),
            "untracked=No must omit untracked, got {:?}",
            model.untracked
        );
        assert!(
            model.ignored.iter().any(|p| p == "noise.log"),
            "ignored must still be collected with untracked=No, got {:?}",
            model.ignored
        );
    }

    #[test]
    fn status_untracked_mode_no_skips_walk() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join("foo.txt"), b"hi\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            untracked: UntrackedMode::No,
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        assert!(
            model.untracked.is_empty(),
            "untracked=No must report nothing, got {:?}",
            model.untracked
        );
    }

    #[test]
    fn warm_status_reuses_populated_untr_without_rescanning_worktree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        const FILE_COUNT: usize = 4500;
        for i in 0..FILE_COUNT {
            let depth = 3 + (i % 5);
            let mut rel = String::new();
            for d in 0..depth {
                rel.push_str(&format!("/d{}", (i + d) % 7));
            }
            rel.push_str(&format!("/f{i}.txt"));
            rel.remove(0);
            let abs = root.join(&rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&abs, format!("body {i}\n")).unwrap();
        }
        grit_test_support::git(root, &["add", "-A"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();

        untracked_walk_probe::reset();
        let _ = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let entries_read = untracked_walk_probe::count();
        assert!(
            entries_read <= 32,
            "populated UNTR must answer warm status without a full worktree rescan \
             (read {entries_read} directory entries)"
        );
    }

    #[test]
    fn conflict_kind_from_stages_both_modified() {
        assert_eq!(
            conflict_kind_from_stages(true, true, true),
            ConflictKind::BothModified
        );
    }

    #[test]
    fn conflict_kind_from_stages_both_added() {
        assert_eq!(
            conflict_kind_from_stages(false, true, true),
            ConflictKind::BothAdded
        );
    }

    #[test]
    fn conflict_kind_from_stages_both_deleted() {
        assert_eq!(
            conflict_kind_from_stages(true, false, false),
            ConflictKind::BothDeleted
        );
    }

    #[test]
    fn conflict_kind_from_stages_added_by_us() {
        assert_eq!(
            conflict_kind_from_stages(false, true, false),
            ConflictKind::AddedByUs
        );
    }

    #[test]
    fn conflict_kind_from_stages_added_by_them() {
        assert_eq!(
            conflict_kind_from_stages(false, false, true),
            ConflictKind::AddedByThem
        );
    }

    #[test]
    fn conflict_kind_from_stages_deleted_by_us() {
        assert_eq!(
            conflict_kind_from_stages(true, false, true),
            ConflictKind::DeletedByUs
        );
    }

    #[test]
    fn conflict_kind_from_stages_deleted_by_them() {
        assert_eq!(
            conflict_kind_from_stages(true, true, false),
            ConflictKind::DeletedByThem
        );
    }

    #[test]
    fn operation_next_steps_merge() {
        use crate::state::{operation_next_steps, InProgressOperation, NextStep};

        let tmp = merge_conflict_repo_for_tests();
        let root = tmp.path();
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let op = model.operation.expect("merge operation");
        assert_eq!(op.kind, InProgressOperation::Merge);
        assert_eq!(
            op.next_steps,
            operation_next_steps(InProgressOperation::Merge, true)
        );
        assert!(op.next_steps.contains(&NextStep::ResolveConflicts));
        assert!(!op
            .next_steps
            .iter()
            .any(|s| matches!(s, NextStep::Abort(_))));
        assert_eq!(
            op.alternatives,
            crate::state::operation_alternatives(InProgressOperation::Merge)
        );
    }

    #[test]
    fn operation_next_steps_resolved_merge_is_commit_only() {
        use crate::state::{
            operation_alternatives, operation_next_steps, InProgressOperation, NextStep,
        };

        let tmp = merge_conflict_repo_for_tests();
        let root = tmp.path();
        std::fs::write(root.join("f"), "resolved\n").unwrap();
        grit_test_support::git(root, &["add", "f"]);
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let op = model.operation.expect("merge operation");
        assert!(model.conflicts.is_empty());
        assert_eq!(
            op.next_steps,
            operation_next_steps(InProgressOperation::Merge, false)
        );
        assert_eq!(
            op.next_steps,
            vec![NextStep::Continue(InProgressOperation::Merge)]
        );
        assert!(!op
            .next_steps
            .iter()
            .any(|s| matches!(s, NextStep::Abort(_))));
        assert_eq!(
            op.alternatives,
            operation_alternatives(InProgressOperation::Merge)
        );
    }

    #[test]
    fn status_operation_git_cherry_pick_conflict_shape() {
        use crate::state::{
            operation_alternatives, operation_next_steps, InProgressOperation, NextStep,
        };

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main"]);
        grit_test_support::git(root, &["config", "user.name", "T"]);
        grit_test_support::git(root, &["config", "user.email", "t@e.com"]);
        std::fs::write(root.join("f"), "base\n").unwrap();
        grit_test_support::git(root, &["add", "f"]);
        grit_test_support::git(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("f"), "main\n").unwrap();
        grit_test_support::git(root, &["commit", "-qam", "main"]);
        grit_test_support::git(root, &["checkout", "-q", "HEAD~1"]);
        std::fs::write(root.join("f"), "other\n").unwrap();
        grit_test_support::git(root, &["commit", "-qam", "other"]);
        let pick = std::process::Command::new("git")
            .args(["cherry-pick", "main"])
            .current_dir(root)
            .output()
            .expect("cherry-pick");
        assert_ne!(pick.status.code(), Some(0), "expected cherry-pick conflict");

        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        assert!(model.state.cherry_pick_in_progress);
        assert!(!model.conflict_details.is_empty());
        let op = model.operation.expect("cherry-pick operation");
        assert_eq!(op.kind, InProgressOperation::CherryPick);
        assert_eq!(
            op.next_steps,
            operation_next_steps(InProgressOperation::CherryPick, true)
        );
        assert!(op.next_steps.contains(&NextStep::ResolveConflicts));
        assert!(op.next_steps.contains(&NextStep::StageResolved));
        assert!(op
            .next_steps
            .contains(&NextStep::Continue(InProgressOperation::CherryPick)));
        assert!(!op
            .next_steps
            .iter()
            .any(|s| matches!(s, NextStep::Abort(_))));
        assert_eq!(
            op.alternatives,
            operation_alternatives(InProgressOperation::CherryPick)
        );
        assert!(op
            .alternatives
            .contains(&NextStep::Abort(InProgressOperation::CherryPick)));
        assert!(op
            .alternatives
            .contains(&NextStep::Skip(InProgressOperation::CherryPick)));
        assert!(!op.next_steps.iter().any(|s| matches!(s, NextStep::Commit)));
    }

    #[test]
    fn conflicts_from_index_non_utf8_path_kind() {
        use crate::index::{Index, IndexEntry, MODE_REGULAR};

        let mut index = Index::new();
        let path = vec![0xff, b'a'];
        let o1 = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let o2 = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let o3 = ObjectId::from_hex("3333333333333333333333333333333333333333").unwrap();
        for (stage, oid) in [(1u8, o1), (2, o2), (3, o3)] {
            let mut entry = IndexEntry {
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
                oid,
                flags: 0,
                flags_extended: None,
                path: path.clone(),
                base_index_pos: 0,
            };
            entry.set_stage(stage);
            index.add_or_replace(entry);
        }
        let conflicts = conflicts_from_index(&index);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::BothModified);
    }

    fn merge_conflict_repo_for_tests() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main"]);
        grit_test_support::git(root, &["config", "user.name", "T"]);
        grit_test_support::git(root, &["config", "user.email", "t@e.com"]);
        std::fs::write(root.join("f"), "base\n").unwrap();
        grit_test_support::git(root, &["add", "f"]);
        grit_test_support::git(root, &["commit", "-qm", "base"]);
        grit_test_support::git(root, &["checkout", "-qb", "side"]);
        std::fs::write(root.join("f"), "side\n").unwrap();
        grit_test_support::git(root, &["commit", "-qam", "side"]);
        grit_test_support::git(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("f"), "main\n").unwrap();
        grit_test_support::git(root, &["commit", "-qam", "main"]);
        let merge = std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(root)
            .output()
            .expect("git merge");
        assert_ne!(merge.status.code(), Some(0), "expected merge conflict");
        tmp
    }

    #[test]
    fn status_options_from_config_renames_off() {
        let cfg_dir = tempfile::TempDir::new().unwrap();
        fs::write(
            cfg_dir.path().join("config"),
            "[status]\n\trenames = false\n",
        )
        .unwrap();
        let cfg = ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(cfg_dir.path()),
            true,
        )
        .unwrap();
        let opts = status_options_from_config(&cfg).unwrap();
        assert_eq!(opts.renames, None);
    }

    #[test]
    fn status_pathspec_limits_all_sections() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::create_dir_all(root.join("scoped")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("scoped/tracked.txt"), b"v1\n").unwrap();
        fs::write(root.join("other/outside.txt"), b"x\n").unwrap();
        grit_test_support::git(root, &["add", "scoped/tracked.txt", "other/outside.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        fs::write(root.join("scoped/tracked.txt"), b"v2\n").unwrap();
        fs::write(root.join("scoped/staged.txt"), b"new staged\n").unwrap();
        grit_test_support::git(root, &["add", "scoped/staged.txt"]);
        fs::write(root.join("scoped/untracked.txt"), b"?\n").unwrap();
        fs::write(root.join("other/untracked-out.txt"), b"?\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            pathspecs: vec!["scoped".into()],
            renames: Some(RenameDetection {
                threshold: 50,
                copies: false,
            }),
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();

        assert!(
            model.staged.iter().all(|e| e.path().starts_with("scoped/")),
            "staged must be scoped: {:?}",
            model.staged
        );
        assert!(
            model
                .unstaged
                .iter()
                .all(|e| e.path().starts_with("scoped/")),
            "unstaged must be scoped: {:?}",
            model.unstaged
        );
        assert!(
            model
                .untracked
                .iter()
                .all(|p| p.starts_with("scoped/") || p == "scoped/"),
            "untracked must be scoped: {:?}",
            model.untracked
        );
        assert!(
            !model.untracked.iter().any(|p| p.contains("other/")),
            "outside untracked must be omitted"
        );
    }

    #[test]
    fn short_status_lines_keep_rename_endpoints_when_filename_contains_arrow() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a -> b.txt"), b"x\n").unwrap();
        grit_test_support::git(root, &["add", "a -> b.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        grit_test_support::git(root, &["mv", "a -> b.txt", "c -> d.txt"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            renames: Some(RenameDetection {
                threshold: 50,
                copies: false,
            }),
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        let rename_line = short_status_lines(&model)
            .into_iter()
            .find_map(|line| match line {
                StatusShortLine::Tracked { path, .. } => Some(path),
                _ => None,
            })
            .expect("staged rename line");
        assert_eq!(
            rename_line,
            StatusShortPath::Rename {
                old: "a -> b.txt".into(),
                new: "c -> d.txt".into(),
            }
        );
    }

    #[test]
    fn status_rename_staged_reports_old_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a.txt"), b"a\n").unwrap();
        grit_test_support::git(root, &["add", "a.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        grit_test_support::git(root, &["mv", "a.txt", "b.txt"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            renames: Some(RenameDetection {
                threshold: 50,
                copies: false,
            }),
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        let renamed: Vec<_> = model
            .staged
            .iter()
            .filter(|e| e.status == crate::diff::DiffStatus::Renamed)
            .collect();
        assert_eq!(
            renamed.len(),
            1,
            "expected one staged rename: {:?}",
            model.staged
        );
        assert_eq!(renamed[0].old_path.as_deref(), Some("a.txt"));
        assert_eq!(renamed[0].new_path.as_deref(), Some("b.txt"));
    }

    fn staged_letters(model: &StatusModel) -> Vec<(char, String)> {
        model
            .staged
            .iter()
            .map(|e| (e.status.letter(), e.path().to_owned()))
            .collect()
    }

    fn git_staged_porcelain_v2(root: &std::path::Path, pathspec: &str) -> Vec<(char, String)> {
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain=v2", "--", pathspec])
            .current_dir(root)
            .output()
            .expect("git status");
        assert!(
            out.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut rows = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.first() == Some(&"1") {
                if let (Some(xy), Some(path)) = (parts.get(1), parts.get(8)) {
                    let x = xy.chars().next().unwrap_or('.');
                    if x != '.' {
                        rows.push((x, (*path).to_owned()));
                    }
                }
            }
        }
        rows
    }

    #[test]
    fn status_rename_pathspec_matches_git_old_new_and_both() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("old.txt"), b"x\n").unwrap();
        grit_test_support::git(root, &["add", "old.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        grit_test_support::git(root, &["mv", "old.txt", "new.txt"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let renames = Some(RenameDetection {
            threshold: 50,
            copies: false,
        });

        for spec in ["old.txt", "new.txt"] {
            let opts = StatusOptions {
                pathspecs: vec![spec.into()],
                renames,
                ..StatusOptions::default()
            };
            let model = status(&repo, &opts, &mut NullProgress).unwrap();
            let grit_rows = staged_letters(&model);
            let git_rows = git_staged_porcelain_v2(root, spec);
            assert_eq!(
                grit_rows, git_rows,
                "staged rows for pathspec {spec:?} must match git porcelain v2"
            );
        }

        let opts_both = StatusOptions {
            pathspecs: vec![".".into()],
            renames,
            ..StatusOptions::default()
        };
        let model_both = status(&repo, &opts_both, &mut NullProgress).unwrap();
        assert!(
            model_both
                .staged
                .iter()
                .any(|e| e.status == crate::diff::DiffStatus::Renamed),
            "full tree should show rename: {:?}",
            model_both.staged
        );
    }

    #[test]
    fn status_rename_limit_honors_status_key_like_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        for (name, body) in [("a.txt", b"line one\n"), ("b.txt", b"line two\n")] {
            fs::write(root.join(name), body).unwrap();
        }
        grit_test_support::git(root, &["add", "a.txt", "b.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        fs::remove_file(root.join("a.txt")).unwrap();
        fs::remove_file(root.join("b.txt")).unwrap();
        fs::write(root.join("c.txt"), b"line one\n").unwrap();
        fs::write(root.join("d.txt"), b"line two\n").unwrap();
        grit_test_support::git(root, &["add", "c.txt", "d.txt"]);
        grit_test_support::git(root, &["config", "status.renameLimit", "1"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let config = repo.config().expect("config");
        let opts = StatusOptions {
            renames: status_renames_from_config(config.as_ref()),
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        let grit_letters: Vec<_> = staged_letters(&model).into_iter().map(|(c, _)| c).collect();
        let git_out = std::process::Command::new("git")
            .args(["status", "--porcelain=v2"])
            .current_dir(root)
            .output()
            .expect("git status");
        let git_letters: Vec<char> = String::from_utf8_lossy(&git_out.stdout)
            .lines()
            .filter_map(|line| {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.first() != Some(&"1") {
                    return None;
                }
                let x = parts.get(1)?.chars().next()?;
                (x != '.').then_some(x)
            })
            .collect();
        assert_eq!(
            grit_letters, git_letters,
            "status.renameLimit=1 must match git staged letters"
        );
    }

    #[test]
    fn status_unstaged_delete_and_untracked_add_not_rename() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("gone.txt"), b"x\n").unwrap();
        grit_test_support::git(root, &["add", "gone.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::write(root.join("fresh.txt"), b"y\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            renames: Some(RenameDetection {
                threshold: 50,
                copies: false,
            }),
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        assert!(
            model
                .unstaged
                .iter()
                .any(|e| e.status == crate::diff::DiffStatus::Deleted && e.path() == "gone.txt"),
            "deleted tracked file: {:?}",
            model.unstaged
        );
        assert!(
            model.untracked.iter().any(|p| p == "fresh.txt"),
            "untracked add stays untracked: {:?}",
            model.untracked
        );
        assert!(
            !model
                .unstaged
                .iter()
                .any(|e| e.status == crate::diff::DiffStatus::Renamed),
            "must not fold delete+untracked into rename: {:?}",
            model.unstaged
        );
    }
    fn warm_status_after_git_status_reuses_untr_without_rescanning() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        const FILE_COUNT: usize = 4500;
        for i in 0..FILE_COUNT {
            let depth = 3 + (i % 5);
            let mut rel = String::new();
            for d in 0..depth {
                rel.push_str(&format!("/d{}", (i + d) % 7));
            }
            rel.push_str(&format!("/f{i}.txt"));
            rel.remove(0);
            let abs = root.join(&rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&abs, format!("body {i}\n")).unwrap();
        }
        grit_test_support::git(root, &["add", "-A"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();

        grit_test_support::git(root, &["status"]);

        untracked_walk_probe::reset();
        let _ = status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        let entries_read = untracked_walk_probe::count();
        assert!(
            entries_read <= 32,
            "after system git status, grit must reuse UNTR without a full rescan \
             (read {entries_read} directory entries)"
        );
    }

    #[test]
    fn ignored_listing_matches_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(
            root.join(".gitignore"),
            "*.log\nbuild/\n!/keep.log\nignored-dir/\n",
        )
        .unwrap();
        fs::write(root.join("tracked.txt"), b"ok\n").unwrap();
        fs::write(root.join("noise.log"), b"log\n").unwrap();
        fs::create_dir_all(root.join("build/out")).unwrap();
        fs::write(root.join("build/out/x.txt"), b"x\n").unwrap();
        fs::create_dir_all(root.join("ignored-dir/deep")).unwrap();
        fs::write(root.join("ignored-dir/deep/y.log"), b"y\n").unwrap();
        fs::write(root.join("keep.log"), b"keep\n").unwrap();
        grit_test_support::git(root, &["add", "tracked.txt"]);
        grit_test_support::git(root, &["commit", "-qm", "seed"]);

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let opts = StatusOptions {
            ignored: IgnoredMode::Traditional,
            ..StatusOptions::default()
        };
        let model = status(&repo, &opts, &mut NullProgress).unwrap();
        let grit_ignored: std::collections::BTreeSet<String> = model.ignored.into_iter().collect();

        let git_out = std::process::Command::new("git")
            .args(["status", "--ignored", "--porcelain"])
            .current_dir(root)
            .output()
            .expect("git status");
        assert!(
            git_out.status.success(),
            "git status failed:\n{}",
            String::from_utf8_lossy(&git_out.stderr)
        );
        let git_ignored: std::collections::BTreeSet<String> =
            String::from_utf8_lossy(&git_out.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("!! ").map(str::to_owned))
                .collect();

        assert_eq!(
            grit_ignored, git_ignored,
            "ignored paths must match git status --ignored --porcelain (!! lines)"
        );
    }

    #[test]
    fn split_tracked_untracked_tasks_only_splits_plain_recursions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main", "."]);
        fs::create_dir_all(root.join("pkg/a/inner")).unwrap();
        fs::write(root.join("pkg/a/f1"), b"1\n").unwrap();
        fs::write(root.join("pkg/a/inner/f2"), b"2\n").unwrap();
        git(&["add", "pkg"]);
        git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,pkg/gl",
        ]);
        fs::create_dir_all(root.join("pkg/gl")).unwrap();
        fs::write(root.join("pkg/gl/in-sub"), b"s\n").unwrap();
        fs::create_dir_all(root.join("pkg/u")).unwrap();
        fs::write(root.join("pkg/u/new"), b"n\n").unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let index = repo.load_index().unwrap();
        let rules = crate::worktree_rules::WorktreeRules::from_repository(&repo, &index).unwrap();
        let mut matcher = rules.ignore().clone();
        let tracked =
            crate::path_icase::Stage0TrackedPaths::from_index_for_untracked_scan(&index, false);
        let gitlinks: HashSet<String> = ["pkg/gl".to_owned()].into_iter().collect();
        let root_tasks: Vec<UntrackedEntryTask> = named_entries_sorted(fs::read_dir(root).unwrap())
            .into_iter()
            .filter(|(name, _)| name.as_slice() == b"pkg")
            .map(|(name, entry)| UntrackedEntryTask {
                parent: Arc::from(""),
                name,
                entry,
            })
            .collect();

        let tasks = split_tracked_untracked_tasks(
            root_tasks,
            usize::MAX,
            &repo,
            &index,
            &gitlinks,
            false,
            &tracked,
            &mut matcher,
        );
        let queued: Vec<(String, String)> = tasks
            .iter()
            .map(|t| {
                (
                    t.parent.to_string(),
                    String::from_utf8_lossy(&t.name).into_owned(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = [
            ("pkg/a", "f1"),
            ("pkg/a/inner", "f2"),
            ("pkg", "gl"),
            ("pkg", "u"),
        ]
        .into_iter()
        .map(|(p, n)| (p.to_owned(), n.to_owned()))
        .collect();
        assert_eq!(
            queued, expected,
            "tracked directories split in order; the gitlink and the untracked directory stay whole"
        );

        let one_round = split_tracked_untracked_tasks(
            named_entries_sorted(fs::read_dir(root).unwrap())
                .into_iter()
                .filter(|(name, _)| name.as_slice() == b"pkg")
                .map(|(name, entry)| UntrackedEntryTask {
                    parent: Arc::from(""),
                    name,
                    entry,
                })
                .collect(),
            2,
            &repo,
            &index,
            &gitlinks,
            false,
            &tracked,
            &mut matcher,
        );
        assert_eq!(
            one_round.len(),
            3,
            "splitting stops once enough directories are queued"
        );
    }
}

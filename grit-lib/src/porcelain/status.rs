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

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs::{self, DirEntry, ReadDir};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::ConfigSet;
use crate::diff::DiffEntry;
use crate::error::Result;
use crate::hash::{index_parallelism_from_config, try_par_hash_with, ParallelHashError};
use crate::ignore::IgnoreMatcher;
use crate::index::{Index, MODE_GITLINK, MODE_TREE};
use crate::objects::ObjectId;
use crate::precompose_config::effective_core_precomposeunicode_with_config;
use crate::repo::Repository;
use crate::state::{HeadState, WtStatusState};
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
#[derive(Debug, Clone, Copy)]
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
        }
    }
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
        },
        &rules,
    )
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
) -> Result<(Vec<String>, Vec<String>)> {
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
        },
        rules,
    )
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
}

/// Shared implementation of the untracked/ignored walk.
pub(crate) fn collect_untracked_and_ignored_inner(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    pathspecs: &[String],
    scan: UntrackedScan,
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<(Vec<String>, Vec<String>)> {
    let UntrackedScan {
        ignored_mode,
        show_all,
        sort_paths,
        use_untracked_cache,
    } = scan;
    // Keep parity with historical status behavior in tests that rely on broad untracked scans
    // (including detached-HEAD wtstatus cases): when no explicit pathspec is requested, avoid
    // pathspec-based pruning entirely.
    let effective_pathspecs: &[String] = if pathspecs.is_empty() { &[] } else { pathspecs };

    if use_untracked_cache && effective_pathspecs.is_empty() && ignored_mode == IgnoredMode::No {
        let config = rules.config_arc();
        let cache_config = config
            .get_bool("core.untrackedCache")
            .and_then(|r| r.ok())
            .unwrap_or(false);
        let cache_on_index = index.untracked_cache.is_some();
        if cache_config && cache_on_index {
            let ident = crate::untracked_cache::untracked_cache_ident(work_tree);
            let mut uc = index.untracked_cache.take().unwrap_or_else(|| {
                crate::untracked_cache::UntrackedCache::new_shell(0, ident.clone())
            });
            if uc.ident == ident {
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
                return Ok((untracked, Vec::new()));
            }
            index.untracked_cache = Some(uc);
        }
    }
    let ignorecase = rules
        .config()
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let tracked_paths =
        crate::path_icase::Stage0TrackedPaths::from_index_for_untracked_scan(index, ignorecase);
    let tracked: BTreeSet<String> = index
        .entries
        .iter()
        .filter(|ie| ie.stage() == 0)
        .map(|ie| String::from_utf8_lossy(&ie.path).to_string())
        .collect();

    let gitlinks: BTreeSet<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();

    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let precompose_unicode =
        effective_core_precomposeunicode_with_config(Some(&repo.git_dir), Some(rules.config()));

    if !sort_paths && effective_pathspecs.is_empty() && tracked.is_empty() && gitlinks.is_empty() {
        untracked = collect_untracked_parallel_top_level(
            repo,
            index,
            work_tree,
            ignored_mode,
            show_all,
            precompose_unicode,
            &tracked_paths,
            &rules.ignore(),
            effective_pathspecs,
        )?;
        return Ok((untracked, ignored));
    }

    visit_untracked_node(
        repo,
        index,
        work_tree,
        &tracked,
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
    Ok((untracked, ignored))
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
        let narrow = if pathspecs.is_empty() {
            vec![dir.clone()]
        } else {
            pathspecs.to_vec()
        };
        let (files, _) = collect_untracked_and_ignored_with_rules(
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

/// Parallel untracked scan for an empty index (bulk `git add .`), preserving path order loosely.
///
/// Only used without pathspecs, so the walk never needs attribute rules (`rules = None`); the
/// per-operation [`crate::worktree_rules::WorktreeRules`] is not shareable across workers.
#[allow(clippy::too_many_arguments)]
fn collect_untracked_parallel_top_level(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    matcher_template: &IgnoreMatcher,
    pathspecs: &[String],
) -> Result<Vec<String>> {
    let empty_tracked = BTreeSet::<String>::new();
    let empty_gitlinks = BTreeSet::<String>::new();
    let entries: Vec<DirEntry> = fs::read_dir(work_tree)
        .map_err(crate::error::Error::Io)?
        .filter_map(|e| e.ok())
        .collect();

    let mut root_files = Vec::new();
    let mut root_dirs = Vec::new();
    for entry in entries {
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.path().is_dir() {
            root_dirs.push(entry);
        } else {
            root_files.push(entry);
        }
    }
    root_files.sort_by_key(|e| e.file_name());
    root_dirs.sort_by_key(|e| e.file_name());

    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let mut matcher = matcher_template.clone();
    for entry in root_files {
        let _ = visit_untracked_dir_entry(
            repo,
            index,
            work_tree,
            &empty_tracked,
            &empty_gitlinks,
            &mut matcher,
            ignored_mode,
            show_all,
            false,
            None::<&Cell<bool>>,
            precompose_unicode,
            tracked_paths,
            "",
            &entry,
            pathspecs,
            None,
            &mut untracked,
            &mut ignored,
        )?;
    }

    if root_dirs.is_empty() {
        return Ok(untracked);
    }

    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let parallelism = index_parallelism_from_config(&config);
    let threads = parallelism.threads();
    let dir_count = root_dirs.len();
    let est_bytes = dir_count.saturating_mul(4096);
    let per_dir = try_par_hash_with(&root_dirs, threads, est_bytes, |entry| {
        let mut matcher = matcher_template.clone();
        let mut local_untracked = Vec::new();
        let mut local_ignored = Vec::new();
        let _ = visit_untracked_dir_entry(
            repo,
            index,
            work_tree,
            &empty_tracked,
            &empty_gitlinks,
            &mut matcher,
            ignored_mode,
            show_all,
            false,
            None::<&Cell<bool>>,
            precompose_unicode,
            tracked_paths,
            "",
            entry,
            pathspecs,
            None,
            &mut local_untracked,
            &mut local_ignored,
        )?;
        Ok(local_untracked)
    })
    .map_err(|e: ParallelHashError<crate::error::Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;

    for mut local in per_dir {
        untracked.append(&mut local);
    }
    Ok(untracked)
}

#[allow(clippy::too_many_arguments)]
fn visit_untracked_dir_entry(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: IgnoredMode,
    show_all: bool,
    check_only: bool,
    visible_out: Option<&Cell<bool>>,
    precompose_unicode: bool,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel: &str,
    entry: &DirEntry,
    pathspecs: &[String],
    rules: Option<&crate::worktree_rules::WorktreeRules>,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<UntrackedWalkStep> {
    let raw_name = entry.file_name().to_string_lossy().to_string();
    if raw_name == ".git" {
        return Ok(UntrackedWalkStep::Continue);
    }
    let name = if precompose_unicode {
        precompose_utf8_segment(&raw_name).into_owned()
    } else {
        raw_name
    };
    let path = entry.path();
    let child_rel = relative_path(rel, &name);
    let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

    if is_dir && gitlinks.contains(&child_rel) {
        return Ok(UntrackedWalkStep::Continue);
    }

    if tracked_paths.contains(&child_rel) {
        return Ok(UntrackedWalkStep::Continue);
    }

    if is_dir {
        if !pathspec_may_match_directory(&child_rel, pathspecs) {
            return Ok(UntrackedWalkStep::Continue);
        }
        visit_untracked_directory(
            repo,
            index,
            work_tree,
            tracked,
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
        if !status_path_matches_worktree_with_rules(
            repo, index, work_tree, &child_rel, pathspecs, rules,
        ) {
            return Ok(UntrackedWalkStep::Continue);
        }
        let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
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
    entries: ReadDir,
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
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
        if matches!(
            visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                tracked,
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
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
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

    if !rel.is_empty()
        && abs.is_dir()
        && dir_is_nested_submodule_worktree(&repo.git_dir, abs)
        && has_tracked_under(tracked, gitlinks, rel)
    {
        return Ok(());
    }

    let entries = match fs::read_dir(abs) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    if check_only {
        return visit_untracked_read_dir_lazy(
            entries,
            repo,
            index,
            work_tree,
            tracked,
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

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in &sorted {
        if matches!(
            visit_untracked_dir_entry(
                repo,
                index,
                work_tree,
                tracked,
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
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
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
    if has_tracked_under(tracked, gitlinks, rel) {
        visit_untracked_node(
            repo,
            index,
            work_tree,
            tracked,
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
    if ignored_mode == IgnoredMode::No && matcher.check_path(repo, Some(index), rel, true)?.0 {
        return Ok(());
    }

    // Git `dir.c`: with `--ignored=matching` and full untracked listing, an excluded
    // directory is reported as a single path without enumerating children (unless
    // tracked files force a full walk — handled above).
    if ignored_mode == IgnoredMode::Matching
        && show_all
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        ignored_out.push(format!("{rel}/"));
        return Ok(());
    }

    if ignored_mode != IgnoredMode::No
        && dir_is_nested_submodule_worktree(&repo.git_dir, abs)
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        ignored_out.push(format!("{rel}/"));
        return Ok(());
    }

    if ignored_mode == IgnoredMode::Traditional
        && !show_all
        && directory_pathspec_matches_self(rel, pathspecs)
    {
        if let Some(dir_line) = traditional_normal_directory_only(
            repo, index, work_tree, tracked, gitlinks, matcher, rel, abs, pathspecs, rules,
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
            tracked,
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
            tracked,
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
        tracked,
        gitlinks,
        matcher,
        ignored_mode,
        true,
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
        let dir_excluded = matcher.check_path(repo, Some(index), rel, true)?.0;
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
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
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
            if tracked.contains(&rel_child) {
                return Ok(None);
            }
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir && gitlinks.contains(&rel_child) {
                continue;
            }
            if is_dir {
                stack.push(path);
            } else {
                any_file = true;
                let (ig, _) = matcher.check_path(repo, Some(index), &rel_child, false)?;
                if !ig {
                    return Ok(None);
                }
            }
        }
    }

    let dir_ignored = matcher.check_path(repo, Some(index), rel, true)?.0;
    if !any_file {
        return Ok(if dir_ignored {
            Some(format!("{rel}/"))
        } else {
            None
        });
    }

    Ok(Some(format!("{rel}/")))
}

fn has_tracked_under(
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    rel_dir: &str,
) -> bool {
    let prefix = if rel_dir.is_empty() {
        String::new()
    } else {
        format!("{rel_dir}/")
    };
    tracked
        .range::<String, _>(prefix.clone()..)
        .next()
        .is_some_and(|t| t.starts_with(&prefix))
        || gitlinks.iter().any(|g| {
            g.as_str() == rel_dir || (!rel_dir.is_empty() && g.starts_with(&format!("{rel_dir}/")))
        })
}

fn relative_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
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
                    work_tree, normalized, index, &repo.odb,
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

/// Collect repository-relative paths that have unmerged index stages (1–3).
#[must_use]
pub fn conflict_paths_from_index(index: &Index) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        let stage = entry.stage();
        if (1_u8..=3).contains(&stage) {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
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
    progress: &mut dyn ProgressSink,
) -> Result<StatusModel> {
    let work_tree = repo.work_tree.as_deref().ok_or_else(|| {
        crate::error::Error::Message("this operation must be run in a work tree".into())
    })?;

    let head = crate::state::resolve_head(&repo.git_dir)?;
    let state = crate::state::wt_status_get_state(&repo.git_dir, &head, true)?;
    let repo_config = repo.config().ok();

    // Load the index, remembering whether it was sparse on disk, then expand
    // sparse-directory placeholders so the diffs see real entries.
    let index_path = repo.index_path();
    let mut index = match Index::load(&index_path) {
        Ok(i) => i,
        Err(crate::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };
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
    let _ = index.expand_sparse_directory_placeholders(&repo.odb);

    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb.read(oid)?;
            Some(crate::objects::parse_commit(&obj.data)?.tree)
        }
        None => None,
    };

    progress.start("status", None);

    // Staged: index vs HEAD tree, narrowed to pathspecs before rename detection.
    let mut staged: Vec<DiffEntry> = if opts.pathspecs.is_empty()
        && opts.renames.is_none()
        && index.entries.iter().all(|e| e.stage() == 0)
        && head_tree
            .as_ref()
            .is_some_and(|t| index.cache_tree_root.as_ref() == Some(t))
        && index
            .cache_tree
            .as_ref()
            .is_some_and(crate::index::CacheTreeNode::is_valid)
    {
        Vec::new()
    } else {
        crate::diff::diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?
            .into_iter()
            .filter(|e| status_path_matches(e.path(), &opts.pathspecs))
            .collect()
    };

    // Unstaged: worktree vs index, narrowed before rename detection.
    let diff_index_mtime = crate::diff::index_mtime_for_diff(&index, index_mtime);
    let (unstaged_raw, index_stat_refresh_changed) =
        crate::diff::diff_index_to_worktree_with_options(
            &repo.odb,
            &mut index,
            work_tree,
            crate::diff::DiffIndexToWorktreeOptions {
                index_mtime: diff_index_mtime,
                ignore_submodule_untracked: opts.untracked == UntrackedMode::No,
                repository_git_dir: Some(repo.git_dir.clone()),
                refresh_index_stat_in_pass: true,
                config: repo_config.clone(),
                stat_parallel_threads: opts.stat_parallel_threads,
                worktree_rules: Some(Arc::clone(&worktree_rules)),
                for_status: true,
                ..Default::default()
            },
        )?;
    if index_stat_refresh_changed && repo.try_write_index(&mut index)? {
        index.source_mtime = crate::index::index_file_mtime(&index_path);
    }
    let mut unstaged: Vec<DiffEntry> = unstaged_raw
        .into_iter()
        .filter(|e| status_path_matches(e.path(), &opts.pathspecs))
        .collect();

    if let Some(rd) = opts.renames {
        staged = apply_status_renames(&repo.odb, staged, rd, head_tree.as_ref())?;
        unstaged = apply_status_renames(&repo.odb, unstaged, rd, head_tree.as_ref())?;
    }

    let conflicts = conflict_paths_from_index(&index);
    normalize_status_conflicts(&mut staged, &mut unstaged, &conflicts);

    let (untracked, ignored) = if opts.untracked == UntrackedMode::No {
        (Vec::new(), Vec::new())
    } else {
        let rules_guard = worktree_rules.lock().map_err(|e| {
            crate::error::Error::Message(format!("worktree rules lock poisoned: {e}"))
        })?;
        collect_untracked_and_ignored_with_rules(
            repo,
            &mut index,
            work_tree,
            opts.ignored,
            opts.untracked == UntrackedMode::All,
            &opts.pathspecs,
            &rules_guard,
        )?
    };

    let stash_count = crate::reflog::read_reflog(&repo.git_dir, "refs/stash")
        .map(|e| e.len())
        .unwrap_or(0);

    progress.finish();

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
        );
    }
    Ok(crate::diff::detect_renames(
        odb,
        None,
        entries,
        rd.threshold,
    ))
}

#[cfg(test)]
mod status_op_tests {
    use super::*;
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
    fn status_detects_untracked_file_on_unborn_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join("foo.txt"), b"hello\n").unwrap();

        let repo = Repository::open(&root.join(".git"), Some(root)).unwrap();
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

        let repo = Repository::open(&root.join(".git"), Some(root)).unwrap();
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

        let repo = Repository::open(&root.join(".git"), Some(root)).unwrap();
        reset_worktree_metadata_probe();
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
    fn status_untracked_mode_no_skips_walk() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join("foo.txt"), b"hi\n").unwrap();

        let repo = Repository::open(&root.join(".git"), Some(root)).unwrap();
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
}

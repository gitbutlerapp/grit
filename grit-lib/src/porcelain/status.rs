//! `git status` as a structured operation.
//!
//! The library computes a [`StatusModel`] — every fact the user-facing output
//! needs, with **no presentation applied** — and the `grit` binary renders it
//! into porcelain v1/v2, short, or long format (applying colour, column layout,
//! path quoting, and the comment prefix). This is the reference example of the
//! library/CLI split described on [`crate::porcelain`].
//!
//! [`status`] computes the model from [`StatusOptions`]: HEAD and its tree, the
//! staged (index-vs-HEAD) and unstaged (index-vs-worktree) diffs, the untracked
//! and ignored path lists, the in-progress operation
//! [`state`](crate::state::WtStatusState), the loaded and sparse-expanded index,
//! and the stash count. [`short_status_lines`] derives the two-column short form
//! from it, and [`format_porcelain_v2`] renders Git's porcelain v2 lines.

use crate::git_path::CanonicalizeExt;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, DirEntry, ReadDir};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::branch_tracking::{BranchStanding, StatusHeader};
use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::diff::{
    diff_index_to_worktree_with_options, is_unregistered_embedded_git_repo, DiffEntry,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{ConfigError, Error, Result};
use crate::hash::{try_par_hash_with, ParallelHashError};
use crate::ignore::IgnoreMatcher;
use crate::index::{index_file_mtime, Index, MODE_GITLINK, MODE_TREE};
use crate::objects::ObjectId;
use crate::operation::{
    conflict_kind_from_stagemask, conflicts_from_index, Conflict, ConflictKind,
};
use crate::parallel::ParallelOp;
use crate::precompose_config::effective_core_precomposeunicode_with_config;
use crate::repo::Repository;
use crate::state::{status_operation, HeadState, NextStep, Operation, WtStatusState};
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

/// Test hook type so [`StatusOptions`] stays `Debug`/`Clone`.
#[derive(Clone)]
#[doc(hidden)]
pub struct BeforeOpportunisticIndexWriteHook(
    #[doc(hidden)] pub std::sync::Arc<dyn Fn() + Send + Sync>,
);

impl std::fmt::Debug for BeforeOpportunisticIndexWriteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<before_opportunistic_index_write>")
    }
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
    /// Current Unix time in seconds, used to decide whether rewriting an index with racy
    /// entries would clear them (deterministic tests). `None` reads the system clock.
    #[doc(hidden)]
    pub now_unix_seconds: Option<i64>,
    /// When `true`, count entries on `refs/stash` (reads the stash reflog).
    pub count_stash: bool,
    /// When `false`, stat refresh and untracked-cache updates stay in memory only: `.git/index`
    /// is not rewritten and interrupted rewrite recovery is not attempted. Embedders that need
    /// read-only status (for example when `GIT_OPTIONAL_LOCKS=0`) set this to `false`; the
    /// default is `true`.
    pub write_index: bool,
    /// Test hook invoked immediately before an opportunistic index write (deterministic races).
    #[doc(hidden)]
    pub before_opportunistic_index_write: Option<BeforeOpportunisticIndexWriteHook>,
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
            now_unix_seconds: None,
            count_stash: false,
            write_index: true,
            before_opportunistic_index_write: None,
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
/// A renderer is a pure function of this model plus the chosen output format.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct StatusModel {
    /// The resolved HEAD (branch, detached, or unborn).
    pub head: HeadState,
    /// Tree OID of the HEAD commit, or `None` on an unborn branch.
    pub head_tree: Option<ObjectId>,
    /// In-progress operation state (merge, rebase, cherry-pick, bisect, …).
    pub state: WtStatusState,
    /// The loaded index (sparse-directory placeholders stay collapsed unless a
    /// pathspec required expanding them). Not serialized: the
    /// per-path results are in the other fields.
    #[cfg_attr(feature = "serde-models", serde(skip))]
    pub index: Index,
    /// Staged changes: the index-vs-HEAD-tree diff.
    pub staged: Vec<DiffEntry>,
    /// Unstaged changes: the index-vs-worktree diff.
    pub unstaged: Vec<DiffEntry>,
    /// Untracked paths (subject to [`StatusOptions::untracked`]).
    pub untracked: Vec<String>,
    /// Untracked nested repositories that would be recorded as gitlinks if staged.
    pub embedded_repositories: Vec<String>,
    /// Ignored paths (subject to [`StatusOptions::ignored`]).
    pub ignored: Vec<String>,
    /// Paths with unmerged index stages (1–3), sorted and deduplicated.
    pub conflicts: Vec<String>,
    /// Unmerged paths with a [`ConflictKind`] derived from index stages 1–3.
    ///
    /// Never [`ConflictKind::RenameRename`] or a directory rename kind: the index does not
    /// record rename pairings.
    pub conflict_details: Vec<Conflict>,
    /// Paused operation and suggested follow-up steps, when any apply.
    pub operation: Option<Operation>,
    /// Number of stash entries (for the optional stash footer / `--show-stash`).
    pub stash_count: usize,
    /// Whether the on-disk index used the sparse-directory format.
    pub index_sparse_on_disk: bool,
    /// Sparse-directory prefixes present in the on-disk index, if any.
    pub sparse_directory_prefixes: Vec<Vec<u8>>,
    /// What [`status`] did about a rewrite apply (rebase, history edit) that was killed after
    /// moving refs, or `None` when none was pending.
    pub interrupted_rewrite: Option<InterruptedRewrite>,
}

/// How [`status`] dealt with an interrupted rewrite apply, from
/// [`crate::history::recover_interrupted_rewrite_apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum InterruptedRewrite {
    /// Refs had already moved; the work-tree checkout was completed and the rewrite stands.
    FinishedCheckout,
    /// The rewrite was undone: refs, index and work-tree paths are back to their pre-apply state.
    RolledBack,
    /// Another process holds `index.lock`, so recovery was left for later. Refs may already point
    /// at the rewritten commits while the work tree still shows the old ones.
    Pending,
}

impl From<crate::history::RewriteApplyRecovery> for InterruptedRewrite {
    fn from(recovery: crate::history::RewriteApplyRecovery) -> Self {
        match recovery {
            crate::history::RewriteApplyRecovery::FinishedCheckout => Self::FinishedCheckout,
            crate::history::RewriteApplyRecovery::RolledBack => Self::RolledBack,
        }
    }
}

/// Unmerged paths that share one [`ConflictKind`], from [`StatusModel::conflict_groups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictGroup {
    /// How the two sides collided for every path in [`Self::paths`].
    pub kind: ConflictKind,
    /// Repository-relative paths, in the model's (sorted) conflict order.
    pub paths: Vec<String>,
}

/// What to do next with the repository, from [`StatusModel::guidance`].
///
/// The variants are ordered by precedence: a paused operation with runnable steps wins, then
/// unresolved conflicts, then an unfinished merge, then everyday actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusGuidance {
    /// A paused operation that grit (or `git`) can advance.
    Operation {
        /// Ordered follow-ups. Every step has a [`NextStep::command`].
        steps: Vec<NextStep>,
        /// The first mutually exclusive option (for example aborting the merge) that has a
        /// [`NextStep::command`], when any.
        alternative: Option<NextStep>,
    },
    /// Unmerged paths while a merge is in progress: resolve them, then continue the merge.
    ResolveConflictsThenContinueMerge,
    /// Unmerged paths outside a merge: resolve them, then stage each resolved path.
    ResolveConflictsThenStage,
    /// A merge is in progress with no unmerged paths left: continue it.
    ContinueMerge,
    /// No paused operation: the everyday actions that apply, in suggestion order (possibly none).
    Actions(Vec<StatusAction>),
}

/// An everyday follow-up suggested by [`StatusGuidance::Actions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusAction {
    /// Unstaged worktree changes exist and can be discarded.
    DiscardWorktreeChanges,
    /// Unstaged or untracked changes exist and can be staged.
    Stage,
    /// Staged changes exist and can be committed.
    Commit,
    /// The branch is behind (and not ahead of) the ref it integrates with: pull from it.
    Pull {
        /// Short name of the upstream, or of the target when no upstream is configured.
        from: String,
    },
}

impl StatusModel {
    /// Whether there is nothing to commit or resolve: no staged, unstaged, untracked or
    /// unmerged paths, and no merge in progress.
    ///
    /// Ignored paths do not count.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty()
            && self.unstaged.is_empty()
            && self.untracked.is_empty()
            && self.conflict_details.is_empty()
            && !self.state.merge_in_progress
    }

    /// Group [`Self::conflict_details`] by [`ConflictKind`], in [`ConflictKind`] order.
    ///
    /// Empty kinds are omitted; paths keep the model's order within each group.
    #[must_use]
    pub fn conflict_groups(&self) -> Vec<ConflictGroup> {
        let mut groups: Vec<ConflictGroup> = Vec::new();
        for conflict in &self.conflict_details {
            let path = conflict.path.clone();
            match groups.binary_search_by(|g| g.kind.cmp(&conflict.kind)) {
                Ok(at) => groups[at].paths.push(path),
                Err(at) => groups.insert(
                    at,
                    ConflictGroup {
                        kind: conflict.kind.clone(),
                        paths: vec![path],
                    },
                ),
            }
        }
        groups
    }

    /// Suggest what to do next, given the branch's target/upstream `standing`.
    ///
    /// # Parameters
    ///
    /// - `standing`: the current branch's comparison with its target and upstream, used for
    ///   [`StatusAction::Pull`] (see [`StatusHeader::pull_integration_ref`]).
    ///
    /// # Returns
    ///
    /// The highest-precedence [`StatusGuidance`]. Steps of [`Self::operation`] without a
    /// runnable command are dropped; when none remain, the conflict and merge fallbacks apply.
    #[must_use]
    pub fn guidance(&self, standing: &BranchStanding) -> StatusGuidance {
        if let Some(op) = &self.operation {
            let steps: Vec<NextStep> = op
                .next_steps
                .iter()
                .filter(|step| step.command().is_some())
                .cloned()
                .collect();
            if !steps.is_empty() {
                let alternative = op
                    .alternatives
                    .iter()
                    .find(|step| step.command().is_some())
                    .cloned();
                return StatusGuidance::Operation { steps, alternative };
            }
        }
        let has_conflicts = !self.conflict_details.is_empty();
        let merging = self.state.merge_in_progress;
        match (has_conflicts, merging) {
            (true, true) => return StatusGuidance::ResolveConflictsThenContinueMerge,
            (true, false) => return StatusGuidance::ResolveConflictsThenStage,
            (false, true) => return StatusGuidance::ContinueMerge,
            (false, false) => {}
        }
        let mut actions = Vec::new();
        if !self.unstaged.is_empty() {
            actions.push(StatusAction::DiscardWorktreeChanges);
        }
        if !self.unstaged.is_empty() || !self.untracked.is_empty() {
            actions.push(StatusAction::Stage);
        }
        if !self.staged.is_empty() {
            actions.push(StatusAction::Commit);
        }
        if let Some(from) = StatusHeader::pull_integration_ref(standing) {
            actions.push(StatusAction::Pull {
                from: from.to_owned(),
            });
        }
        StatusGuidance::Actions(actions)
    }
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
            clean_unit_dirs: None,
        },
        &rules,
        None,
    )
    .map(|(untracked, ignored, _)| (untracked, ignored))
}

/// Count unstaged (index vs work tree) and untracked paths for an index that already matches `HEAD`.
///
/// Used when an index-only commit is refused so callers can tell a clean tree from dirty worktree
/// state (issue #1969). Does not run rename detection or build a full [`StatusModel`].
///
/// # Errors
///
/// Propagates work-tree, diff, and untracked-walk failures.
pub(crate) fn count_unstaged_and_untracked_paths(
    repo: &Repository,
    index: &mut Index,
) -> Result<(usize, usize)> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let rules_arc = Arc::new(Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: false,
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: repo.config().ok(),
        worktree_rules: Some(Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), index, work_tree, diff_opts)?;
    let unstaged_count = unstaged
        .iter()
        .filter(|e| e.status != DiffStatus::Unmerged)
        .count();
    let worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let (untracked, _, _) = collect_untracked_and_ignored_inner(
        repo,
        index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all: false,
            sort_paths: true,
            use_untracked_cache: false,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
            clean_unit_dirs: None,
        },
        &worktree_rules,
        None,
    )?;
    Ok((unstaged_count, untracked.len()))
}

/// Like [`collect_untracked_and_ignored`], reusing an existing
/// [`crate::worktree_rules::WorktreeRules`] so ignore and attribute files already loaded by the
/// same operation are not read again, and checking `cancel_watch` during the walk.
#[allow(clippy::too_many_arguments)]
pub fn collect_untracked_and_ignored_with_rules_and_cancel(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    pathspecs: &[String],
    rules: &crate::worktree_rules::WorktreeRules,
    cancel_watch: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
            clean_unit_dirs: None,
        },
        rules,
        cancel_watch,
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
#[derive(Debug, Clone)]
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
    /// When set, collapsed directories safe for `git clean` without rescanning are recorded here.
    pub clean_unit_dirs: CleanUnitDirHints,
}

/// Collapsed directory paths (`dir/`) that need no second walk for `clean` dry-run.
pub(crate) type CleanUnitDirHints = Option<Arc<Mutex<HashSet<String>>>>;

fn record_clean_unit_dir(hints: &CleanUnitDirHints, rel: &str) {
    let Some(set) = hints else {
        return;
    };
    if rel.is_empty() {
        return;
    }
    if let Ok(mut guard) = set.lock() {
        guard.insert(if rel.ends_with('/') {
            rel.to_owned()
        } else {
            format!("{rel}/")
        });
    }
}

fn take_clean_unit_dirs(hints: &CleanUnitDirHints) -> HashSet<String> {
    hints
        .as_ref()
        .and_then(|set| set.lock().ok())
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Side effects from an untracked/ignored walk beyond the path lists.
#[derive(Debug, Clone, Default)]
pub struct UntrackedCollectMeta {
    /// The index UNTR extension was rebuilt and should be written to disk.
    pub untracked_cache_refreshed: bool,
    /// Collapsed untracked/ignored directories removable as a unit by `clean`.
    pub clean_collapsed_unit: HashSet<String>,
}

fn poll_untracked_cancel(watch: Option<&std::sync::atomic::AtomicBool>) -> Result<()> {
    if let Some(flag) = watch {
        crate::progress::poll_watch(flag)?;
    }
    Ok(())
}

/// Shared implementation of the untracked/ignored walk.
pub(crate) fn collect_untracked_and_ignored_inner(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    pathspecs: &[String],
    scan: UntrackedScan,
    rules: &crate::worktree_rules::WorktreeRules,
    cancel_watch: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(Vec<String>, Vec<String>, UntrackedCollectMeta)> {
    poll_untracked_cancel(cancel_watch.as_deref())?;
    let UntrackedScan {
        ignored_mode,
        show_all,
        sort_paths,
        use_untracked_cache,
        cache_miss,
        clean_unit_dirs,
    } = scan;
    let clean_unit_dirs = &clean_unit_dirs;
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
                    poll_untracked_cancel(cancel_watch.as_deref())?;
                    crate::untracked_cache::refresh_untracked_cache_for_status(
                        repo,
                        index,
                        work_tree,
                        &config,
                        &mut uc,
                        show_all,
                        ignored_mode_to_untracked_cache(ignored_mode),
                        cancel_watch.as_deref(),
                    )?;
                    poll_untracked_cancel(cancel_watch.as_deref())?;
                    let untracked = crate::untracked_cache::collect_untracked_from_cache(&uc);
                    index.untracked_cache = Some(uc);
                    return Ok((
                        untracked,
                        Vec::new(),
                        UntrackedCollectMeta {
                            untracked_cache_refreshed: true,
                            ..UntrackedCollectMeta::default()
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

    let gitlinks = stage0_gitlink_paths(index);

    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    let precompose_unicode =
        effective_core_precomposeunicode_with_config(Some(repo.git_dir()), Some(rules.config()));

    if effective_pathspecs.is_empty() {
        poll_untracked_cancel(cancel_watch.as_deref())?;
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
            rules.config(),
            effective_pathspecs,
            cancel_watch.as_deref(),
            clean_unit_dirs,
        )?;
        if sort_paths {
            par_untracked.sort_unstable();
            par_ignored.sort_unstable();
        }
        return Ok((
            par_untracked,
            par_ignored,
            UntrackedCollectMeta {
                clean_collapsed_unit: take_clean_unit_dirs(clean_unit_dirs),
                ..UntrackedCollectMeta::default()
            },
        ));
    }

    poll_untracked_cancel(cancel_watch.as_deref())?;
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
        cancel_watch.as_deref(),
        clean_unit_dirs,
    )?;

    if sort_paths {
        untracked.sort();
        ignored.sort();
    }
    Ok((
        untracked,
        ignored,
        UntrackedCollectMeta {
            clean_collapsed_unit: take_clean_unit_dirs(clean_unit_dirs),
            ..UntrackedCollectMeta::default()
        },
    ))
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
///
/// Each collapsed directory is walked once, starting at the directory itself, with the full
/// untracked listing; files are kept only when they match `pathspecs`. Ignored directories are
/// dropped and unregistered embedded repositories stay as `dir/` markers. The cost is linear
/// in the size of the collapsed subtrees.
pub fn expand_untracked_for_staging_with_rules(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    untracked: Vec<String>,
    pathspecs: &[String],
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<Vec<String>> {
    let (mut expanded, dirs): (Vec<String>, Vec<String>) =
        untracked.into_iter().partition(|p| !p.ends_with('/'));
    if !dirs.is_empty() {
        let ignorecase = rules
            .config()
            .get_bool("core.ignorecase")
            .and_then(|r| r.ok())
            .unwrap_or(false);
        let tracked_paths =
            crate::path_icase::Stage0TrackedPaths::from_index_for_untracked_scan(index, ignorecase);
        let gitlinks = stage0_gitlink_paths(index);
        let precompose_unicode = effective_core_precomposeunicode_with_config(
            Some(repo.git_dir()),
            Some(rules.config()),
        );
        let mut matcher = rules.ignore_mut();
        let mut ignored = Vec::new();
        for path in dirs {
            let dir = path.trim_end_matches('/');
            if dir.is_empty() {
                continue;
            }
            visit_untracked_directory(
                repo,
                index,
                work_tree,
                &gitlinks,
                &mut matcher,
                IgnoredMode::No,
                true,
                false,
                None,
                precompose_unicode,
                &tracked_paths,
                dir,
                &work_tree.join(dir),
                pathspecs,
                Some(rules),
                &mut expanded,
                &mut ignored,
                None,
                &None,
            )?;
        }
    }
    expanded.sort();
    expanded.dedup();
    Ok(expanded)
}

/// Stored paths of gitlink entries in `index` (any conflict stage).
fn stage0_gitlink_paths(index: &Index) -> HashSet<String> {
    index
        .entries
        .iter()
        .filter(|e| e.mode == MODE_GITLINK)
        .map(|e| crate::repo_relpath::storage_string(&e.path))
        .collect()
}

/// Test-only counters for the serial untracked walk on the current thread: directory
/// entries consumed during check-only probes, and directories listed.
#[cfg(test)]
pub(crate) mod untracked_walk_probe {
    use std::cell::Cell;

    thread_local! {
        static ENTRIES: Cell<u32> = const { Cell::new(0) };
        static DIRS: Cell<u32> = const { Cell::new(0) };
    }

    pub fn reset() {
        ENTRIES.with(|c| c.set(0));
        DIRS.with(|c| c.set(0));
    }

    pub fn count() -> u32 {
        ENTRIES.with(|c| c.get())
    }

    pub fn dirs_read() -> u32 {
        DIRS.with(|c| c.get())
    }

    pub(super) fn record_entry() {
        ENTRIES.with(|c| c.set(c.get().saturating_add(1)));
    }

    pub(super) fn record_dir_read() {
        DIRS.with(|c| c.set(c.get().saturating_add(1)));
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
    config: &ConfigSet,
    pathspecs: &[String],
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
    clean_unit_dirs: &CleanUnitDirHints,
) -> Result<(Vec<String>, Vec<String>)> {
    poll_untracked_cancel(cancel_watch)?;
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
    let parallelism = repo
        .thread_policy()
        .resolve(ParallelOp::IndexRefresh, config)
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
                cancel_watch,
                clean_unit_dirs,
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
                    cancel_watch,
                    clean_unit_dirs,
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
                cancel_watch,
                clean_unit_dirs,
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
        || !has_tracked_under(index, gitlinks, tracked_paths, &rel)
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
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
    clean_unit_dirs: &CleanUnitDirHints,
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
            cancel_watch,
            clean_unit_dirs,
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
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
    clean_unit_dirs: &CleanUnitDirHints,
) -> Result<()> {
    for entry in entries {
        poll_untracked_cancel(cancel_watch)?;
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
                cancel_watch,
                clean_unit_dirs,
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
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
    clean_unit_dirs: &CleanUnitDirHints,
) -> Result<()> {
    if check_only && visible_out.is_some_and(|out| out.get()) {
        return Ok(());
    }

    let entries = match fs::read_dir(abs) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    #[cfg(test)]
    untracked_walk_probe::record_dir_read();

    let tracked_subdir = !rel.is_empty() && has_tracked_under(index, gitlinks, tracked_paths, rel);
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
            cancel_watch,
            clean_unit_dirs,
        );
    }

    if check_only {
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
            cancel_watch,
            clean_unit_dirs,
        );
    }

    let listing = untracked_candidates_sorted(entries, index, rel, precompose_unicode);
    if tracked_subdir
        && listing.has_dot_git
        && dir_is_nested_submodule_worktree(repo.git_dir(), abs)
    {
        return Ok(());
    }

    for (name, entry) in &listing.candidates {
        poll_untracked_cancel(cancel_watch)?;
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
                cancel_watch,
                clean_unit_dirs,
            )?,
            UntrackedWalkStep::Stop
        ) {
            break;
        }
    }

    Ok(())
}

/// One directory's listing for the untracked walk, see [`untracked_candidates_sorted`].
struct UntrackedListing {
    /// Entries that still need [`visit_untracked_dir_entry`], ordered by name bytes.
    candidates: Vec<(Vec<u8>, DirEntry)>,
    /// Whether any entry (left out or not) is named `.git` in any ASCII case.
    has_dot_git: bool,
}

/// Read the directory stored as `rel`, leaving out non-directories tracked at exactly
/// `rel/<name>` (any stage).
///
/// The walk reports nothing for those, and they are nearly every entry of a typical
/// directory: dropping them while reading spares building their path, looking it up in
/// [`crate::path_icase::Stage0TrackedPaths`], and keeping and sorting them. Names are matched
/// against the directory's run of index entries; when the index is not known to be sorted,
/// or a name needs precomposing first, the entry is kept and [`visit_untracked_dir_entry`]
/// settles it as before.
fn untracked_candidates_sorted(
    entries: ReadDir,
    index: &Index,
    rel: &str,
    precompose_unicode: bool,
) -> UntrackedListing {
    let dir = crate::repo_relpath::storage_bytes(rel);
    let tracked = index.sorted_entries_under(&dir).unwrap_or_default();
    let prefix_len = if dir.is_empty() { 0 } else { dir.len() + 1 };
    let mut has_dot_git = false;
    let mut candidates = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = dir_entry_name_bytes(&entry);
        has_dot_git |= name.eq_ignore_ascii_case(b".git");
        let tracked_file = !(precompose_unicode && !name.is_ascii())
            && tracked
                .binary_search_by(|e| e.path[prefix_len..].cmp(name.as_slice()))
                .is_ok()
            && !crate::read_dir_kind::entry_is_dir(&entry).unwrap_or(false);
        if !tracked_file {
            candidates.push((name, entry));
        }
    }
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    UntrackedListing {
        candidates,
        has_dot_git,
    }
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
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
    clean_unit_dirs: &CleanUnitDirHints,
) -> Result<()> {
    matcher.note_directory_known(rel);

    if has_tracked_under(index, gitlinks, tracked_paths, rel) {
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
            cancel_watch,
            clean_unit_dirs,
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

    // An excluded directory cannot re-include descendants; report it once when listing ignored
    // paths in normal mode without enumerating every child (issue #2242). With
    // `--untracked-files=all`, Git lists individual ignored files instead.
    if ignored_mode != IgnoredMode::No
        && !show_all
        && !rel.is_empty()
        && matcher.is_path_ignored(repo, Some(index), rel, true)?
    {
        ignored_out.push(format!("{rel}/"));
        record_clean_unit_dir(clean_unit_dirs, rel);
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
            cancel_watch,
            clean_unit_dirs,
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
            cancel_watch,
            clean_unit_dirs,
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
        cancel_watch,
        clean_unit_dirs,
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

/// Whether `rel_dir` holds tracked entries. With `core.ignorecase`, a directory spelled with
/// different ASCII case in the index counts (`build` when `Build/t` is tracked), so ignore
/// rules never prune it before its tracked files are matched.
fn has_tracked_under(
    index: &Index,
    gitlinks: &HashSet<String>,
    tracked_paths: &crate::path_icase::Stage0TrackedPaths,
    rel_dir: &str,
) -> bool {
    if index.has_stage0_descendant_under(rel_dir)
        || (!rel_dir.is_empty() && tracked_paths.has_dir_ignoring_case(rel_dir.as_bytes()))
    {
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
    let Ok(resolved_canon) = resolved.canonicalize_simplified() else {
        return false;
    };
    let modules_root = super_git_dir.join("modules");
    let Ok(modules_canon) = modules_root.canonicalize_simplified() else {
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
        .any(|spec| crate::pathspec::pathspec_has_attr_magic(spec))
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
    let is_dir_prefix = |dir: &str, path: &str| {
        path.len() > dir.len() && path.as_bytes()[dir.len()] == b'/' && path.starts_with(dir)
    };
    pathspecs.iter().any(|spec| {
        if crate::pathspec::has_glob_chars(spec) {
            return true;
        }
        let spec_norm = spec.trim_end_matches('/');
        if spec_norm == rel_dir
            || is_dir_prefix(rel_dir, spec_norm)
            || is_dir_prefix(spec_norm, rel_dir)
        {
            return true;
        }
        // Without magic or a `./` prefix, a glob-free spec matches only itself and paths
        // under it, which the prefix checks above already cover.
        let plain_literal = !spec_norm.is_empty()
            && !spec.starts_with(':')
            && !spec.starts_with("./")
            && spec != ".";
        !plain_literal && crate::pathspec::pathspec_matches(spec, rel_dir)
    })
}

fn directory_pathspec_matches_self(rel_dir: &str, pathspecs: &[String]) -> bool {
    pathspecs.is_empty()
        || status_path_matches(&format!("{}/", rel_dir.trim_end_matches('/')), pathspecs)
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

fn embedded_unregistered_git_repositories(
    work_tree: &Path,
    index: &Index,
    untracked: &[String],
    precompose_unicode: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for path in untracked {
        let rel = path.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let abs = if precompose_unicode {
            crate::unicode_normalization::resolve_worktree_path_for_staging(work_tree, rel, true)
                .abs
        } else {
            work_tree.join(rel)
        };
        if is_unregistered_embedded_git_repo(index, rel, &abs) {
            out.push(format!("{rel}/"));
        }
    }
    out.sort();
    out.dedup();
    out
}

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
///
/// When [`StatusOptions::write_index`] is `true` and the repository
/// [`Environment`](crate::environment::Environment) allows optional locks, this function may
/// rewrite `.git/index` after refreshing cached stat data (and may finish an interrupted rewrite
/// apply). If another command updates the index after this call loaded it (for example a
/// concurrent commit), the opportunistic write is skipped so the fresher on-disk index is kept.
/// Set `write_index` to `false` or capture `GIT_OPTIONAL_LOCKS=0` in the environment for
/// read-only status that leaves the index file untouched.
pub fn status(
    repo: &Repository,
    opts: &StatusOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<StatusModel> {
    crate::trace::trace_span!("grit.status.scan");
    let persist_index_to_disk = opts.write_index && repo.environment().git_optional_locks;
    let interrupted_rewrite = if persist_index_to_disk {
        match crate::history::recover_interrupted_rewrite_apply(repo) {
            Ok(recovery) => recovery.map(InterruptedRewrite::from),
            Err(crate::history::HistoryError::Library(err))
                if matches!(*err, crate::error::Error::LockHeld { .. }) =>
            {
                Some(InterruptedRewrite::Pending)
            }
            Err(err) => return Err(crate::error::Error::History(err)),
        }
    } else if crate::history::rewrite_apply_journal_active(repo.git_dir()) {
        Some(InterruptedRewrite::Pending)
    } else {
        None
    };
    let watch = crate::progress::cancel_watch_arc(cancel);
    let mut progress = crate::progress::WatchProgress::new(progress, std::sync::Arc::clone(&watch));
    crate::progress::poll_cancel_watch(&mut progress, cancel, &watch)?;
    let work_tree = repo
        .work_tree()
        .ok_or(crate::error::Error::RequiresWorkTree)?;

    let head = crate::state::resolve_head_for_repository(repo)?;
    let state = crate::state::wt_status_get_state(repo.git_dir(), &head, true)?;
    let repo_config = repo.config().ok();

    // Load without expanding sparse-directory placeholders; expand only directories
    // a pathspec may reach so read-mostly status stays on Git's sparse index shape.
    let index_path = repo.index_path_for_env()?;
    let mut index = repo.load_index_at_preserving_sparse(
        &index_path,
        crate::index::IndexParseContext::from_policy(repo.thread_policy()),
    )?;
    let index_mtime = index.source_mtime;
    let sparse_directory_prefixes: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.is_sparse_directory_placeholder())
        .map(|e| e.path.clone())
        .collect();
    let index_sparse_on_disk =
        index.sparse_directories || index.has_sparse_directory_placeholders();
    if !opts.pathspecs.is_empty() {
        crate::porcelain::index_from_tree::expand_sparse_for_pathspecs(
            repo,
            &mut index,
            &opts.pathspecs,
        )?;
    }
    if let Some(cfg) = repo_config.as_deref() {
        repo.clear_present_skip_worktree(&mut index, cfg);
    }
    let worktree_rules = Arc::new(Mutex::new(
        crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?,
    ));

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
    // Rewrite the index when it has racy entries that a rewrite now would clear: the refresh
    // pass below verifies them by content, and a newer index mtime lets later runs trust stat
    // data instead of re-hashing the same files every time. Unlike Git, skip the rewrite while
    // the clock is still in the second of the newest racy entry, where it cannot help.
    let stat_policy = crate::index::StatMatchPolicy::from_config(repo_config.as_deref());
    let now_unix_seconds = opts
        .now_unix_seconds
        .unwrap_or_else(crate::environment::wall_clock_unix_timestamp_seconds);
    let rewrite_clears_racy_entries = index.entries.iter().any(|e| {
        e.mode != crate::index::MODE_GITLINK
            && crate::diff::entry_is_racy_with_policy(e, diff_index_mtime, stat_policy)
            && i64::from(e.mtime_sec) < now_unix_seconds
    });
    crate::progress::poll_cancel_watch(&mut progress, cancel, &watch)?;
    let cancel_poll = std::sync::Arc::new(crate::progress::StackCancelPoll::new(
        &mut progress,
        std::sync::Arc::clone(&watch),
    ));
    let (unstaged_raw, index_stat_refresh_changed) =
        crate::diff::diff_index_to_worktree_with_owner_poll(
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
                cancel_watch: Some(std::sync::Arc::clone(&watch)),
                ..Default::default()
            },
            Some(cancel_poll),
        )?;
    let mut persist_index = index_stat_refresh_changed
        || rewrite_clears_racy_entries
        || index.entry_modes_repaired_on_load;
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
            Some(&watch),
        )?;
        crate::progress::poll_cancel_watch(&mut progress, cancel, &watch)?;
        unstaged = apply_status_renames(
            repo.odb(),
            unstaged,
            rd,
            head_tree.as_ref(),
            &rename_config,
            &thread_policy,
            Some(&watch),
        )?;
    }

    let mut conflict_details = conflicts_from_index(&index);
    if !opts.pathspecs.is_empty() {
        conflict_details.retain(|c| status_path_matches(&c.path, &opts.pathspecs));
    }
    let conflicts: Vec<String> = conflict_details.iter().map(|c| c.path.clone()).collect();
    normalize_status_conflicts(&mut staged, &mut unstaged, &conflicts);

    crate::progress::poll_cancel_watch(&mut progress, cancel, &watch)?;
    let (untracked, ignored) =
        if opts.untracked == UntrackedMode::No && opts.ignored == IgnoredMode::No {
            (Vec::new(), Vec::new())
        } else {
            let rules_guard = worktree_rules.lock().map_err(|e| {
                crate::error::Error::Message(format!("worktree rules lock poisoned: {e}"))
            })?;
            let (mut untracked, ignored, collect_meta) =
                collect_untracked_and_ignored_with_rules_and_cancel(
                    repo,
                    &mut index,
                    work_tree,
                    opts.ignored,
                    opts.untracked == UntrackedMode::All,
                    &opts.pathspecs,
                    &rules_guard,
                    Some(std::sync::Arc::clone(&watch)),
                )?;
            persist_index |= collect_meta.untracked_cache_refreshed;
            if opts.untracked == UntrackedMode::No {
                untracked.clear();
            }
            (untracked, ignored)
        };

    if persist_index && persist_index_to_disk {
        // The refresh pass already re-hashed every racily clean entry and invalidated the
        // ones whose content changed, so the write must not hash them a second time.
        index.skip_racy_smudge_on_write = true;
        if let Some(hook) = &opts.before_opportunistic_index_write {
            (hook.0)();
        }
        let written = repo.try_write_index_at(&index_path, &mut index);
        index.skip_racy_smudge_on_write = false;
        if written? {
            crate::index::advance_index_file_mtime_past_entries(&index, &index_path, None)?;
            index.source_mtime = crate::index::index_file_mtime(&index_path);
            index.entry_modes_repaired_on_load = false;
        }
    }

    let stash_count = match opts.count_stash {
        false => 0,
        true if repo.refs_on_disk() => {
            crate::reflog::reflog_entry_count(repo.git_dir(), "refs/stash").unwrap_or(0)
        }
        true => crate::reflog::read_reflog_on_store(repo.refs(), "refs/stash")
            .map_or(0, |entries| entries.len()),
    };

    let operation = status_operation(&state, !conflicts.is_empty());

    let precompose_unicode = repo_config
        .as_ref()
        .and_then(|c| c.get_bool("core.precomposeunicode").and_then(|r| r.ok()))
        .unwrap_or(false);
    let embedded_repositories =
        embedded_unregistered_git_repositories(work_tree, &index, &untracked, precompose_unicode);

    Ok(StatusModel {
        head,
        head_tree,
        state,
        index,
        staged,
        unstaged,
        untracked,
        embedded_repositories,
        ignored,
        conflicts,
        conflict_details,
        operation,
        stash_count,
        index_sparse_on_disk,
        sparse_directory_prefixes,
        interrupted_rewrite,
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
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<DiffEntry>> {
    use crate::diff::DiffStatus;
    const MATRIX_BUDGET: usize = 50_000;
    const CANDIDATE_LIMIT: usize = 2_000;

    if let Some(watch) = cancel_watch {
        crate::progress::poll_watch(watch)?;
    }
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
            cancel_watch,
        );
    }
    crate::diff::detect_renames_with_policy(
        odb,
        None,
        entries,
        rd.threshold,
        config,
        thread_policy,
        cancel_watch,
    )
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

/// Porcelain short-status letters for an unmerged path's stage bitmask.
fn porcelain_v2_unmerged_xy(stagemask: u8) -> (char, char) {
    let key = match conflict_kind_from_stagemask(stagemask) {
        Some(ConflictKind::BothDeleted) => "DD",
        Some(ConflictKind::AddedByUs) => "AU",
        Some(ConflictKind::DeletedByThem) => "UD",
        Some(ConflictKind::AddedByThem) => "UA",
        Some(ConflictKind::DeletedByUs) => "DU",
        Some(ConflictKind::BothAdded) => "AA",
        Some(
            ConflictKind::BothModified
            | ConflictKind::RenameRename(_)
            | ConflictKind::DirectoryRenameMoved(_)
            | ConflictKind::DirectoryRenameSplit
            | ConflictKind::DirectoryRenameBlocked(_),
        ) => "UU",
        None => {
            debug_assert!(false, "unexpected unmerged stagemask {stagemask:#x}");
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
        if !entry.new_mode.is_absent() {
            return entry.new_mode.to_string();
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
mod guidance_tests {
    use super::*;
    use crate::branch_tracking::{StandingHeader, TrackedComparison};
    use crate::diff::DiffStatus;
    use crate::objects::FileMode;
    use crate::state::{status_operation, InProgressOperation};

    fn model() -> StatusModel {
        StatusModel {
            head: HeadState::Branch {
                refname: "refs/heads/main".to_owned(),
                short_name: "main".to_owned(),
                oid: Some(ObjectId::zero()),
            },
            head_tree: None,
            state: WtStatusState::default(),
            index: Index::default(),
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            embedded_repositories: Vec::new(),
            ignored: Vec::new(),
            conflicts: Vec::new(),
            conflict_details: Vec::new(),
            operation: None,
            stash_count: 0,
            index_sparse_on_disk: false,
            sparse_directory_prefixes: Vec::new(),
            interrupted_rewrite: None,
        }
    }

    fn modified(path: &str) -> DiffEntry {
        DiffEntry {
            status: DiffStatus::Modified,
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::REGULAR,
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: None,
        }
    }

    fn conflict(path: &str, kind: ConflictKind) -> Conflict {
        Conflict {
            path: path.to_owned(),
            kind,
        }
    }

    fn tracked(name: &str, ahead: Option<usize>, behind: Option<usize>) -> TrackedComparison {
        TrackedComparison {
            name: name.to_owned(),
            full_ref: format!("refs/remotes/{name}"),
            oid: ObjectId::zero(),
            ahead,
            behind,
            newest_ahead: Vec::new(),
        }
    }

    fn standing(
        target: Option<TrackedComparison>,
        upstream: Option<TrackedComparison>,
    ) -> BranchStanding {
        BranchStanding {
            branch: Some("main".to_owned()),
            target,
            upstream,
            header: StandingHeader::NoTarget,
        }
    }

    fn merging(model: &mut StatusModel) {
        model.state.merge_in_progress = true;
        model.operation = status_operation(&model.state, !model.conflict_details.is_empty());
    }

    #[test]
    fn empty_model_is_clean_with_no_actions() {
        let m = model();
        assert!(m.is_clean());
        assert!(m.conflict_groups().is_empty());
        assert_eq!(
            m.guidance(&standing(None, None)),
            StatusGuidance::Actions(Vec::new())
        );
    }

    #[test]
    fn ignored_paths_keep_the_model_clean() {
        let mut m = model();
        m.ignored.push("target/".to_owned());
        assert!(m.is_clean());
    }

    #[test]
    fn each_kind_of_change_makes_the_model_dirty() {
        let mut staged = model();
        staged.staged.push(modified("a"));
        let mut unstaged = model();
        unstaged.unstaged.push(modified("a"));
        let mut untracked = model();
        untracked.untracked.push("new".to_owned());
        let mut conflicted = model();
        conflicted
            .conflict_details
            .push(conflict("a", ConflictKind::BothModified));
        let mut merge = model();
        merge.state.merge_in_progress = true;
        for m in [staged, unstaged, untracked, conflicted, merge] {
            assert!(!m.is_clean());
        }
    }

    #[test]
    fn conflict_groups_follow_kind_order_and_keep_path_order() {
        let mut m = model();
        m.conflict_details = vec![
            conflict("a", ConflictKind::DeletedByThem),
            conflict("b", ConflictKind::BothModified),
            conflict("c", ConflictKind::DeletedByThem),
            conflict("d", ConflictKind::BothAdded),
            conflict("e", ConflictKind::BothModified),
        ];
        assert_eq!(
            m.conflict_groups(),
            vec![
                ConflictGroup {
                    kind: ConflictKind::BothModified,
                    paths: vec!["b".to_owned(), "e".to_owned()],
                },
                ConflictGroup {
                    kind: ConflictKind::BothAdded,
                    paths: vec!["d".to_owned()],
                },
                ConflictGroup {
                    kind: ConflictKind::DeletedByThem,
                    paths: vec!["a".to_owned(), "c".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn everyday_actions_come_in_suggestion_order() {
        let mut m = model();
        m.staged.push(modified("s"));
        m.unstaged.push(modified("u"));
        let behind_upstream = standing(None, Some(tracked("origin/main", Some(0), Some(2))));
        assert_eq!(
            m.guidance(&behind_upstream),
            StatusGuidance::Actions(vec![
                StatusAction::DiscardWorktreeChanges,
                StatusAction::Stage,
                StatusAction::Commit,
                StatusAction::Pull {
                    from: "origin/main".to_owned()
                },
            ])
        );
    }

    #[test]
    fn untracked_paths_suggest_staging_but_not_discarding() {
        let mut m = model();
        m.untracked.push("new".to_owned());
        assert_eq!(
            m.guidance(&standing(None, None)),
            StatusGuidance::Actions(vec![StatusAction::Stage])
        );
    }

    #[test]
    fn pull_is_suggested_only_when_strictly_behind() {
        let m = model();
        let cases = [
            (
                standing(Some(tracked("main", Some(0), Some(3))), None),
                true,
            ),
            (
                standing(Some(tracked("main", Some(1), Some(3))), None),
                false,
            ),
            (
                standing(Some(tracked("main", Some(0), Some(0))), None),
                false,
            ),
            (standing(Some(tracked("main", None, None)), None), false),
            (
                standing(
                    Some(tracked("main", Some(0), Some(3))),
                    Some(tracked("origin/topic", Some(0), Some(0))),
                ),
                false,
            ),
        ];
        for (standing, pull) in cases {
            let suggests_pull = matches!(
                m.guidance(&standing),
                StatusGuidance::Actions(actions)
                    if actions.iter().any(|a| matches!(a, StatusAction::Pull { .. }))
            );
            assert_eq!(suggests_pull, pull, "{standing:?}");
        }
    }

    #[test]
    fn paused_merge_with_conflicts_lists_runnable_steps_and_abort() {
        let mut m = model();
        m.conflict_details
            .push(conflict("a", ConflictKind::BothModified));
        merging(&mut m);
        assert_eq!(
            m.guidance(&standing(None, None)),
            StatusGuidance::Operation {
                steps: vec![NextStep::ResolveConflicts, NextStep::StageResolved],
                alternative: Some(NextStep::Abort(InProgressOperation::Merge)),
            }
        );
    }

    #[test]
    fn resolved_merge_suggests_continue() {
        let mut m = model();
        merging(&mut m);
        assert_eq!(
            m.guidance(&standing(None, None)),
            StatusGuidance::Operation {
                steps: vec![NextStep::Continue(InProgressOperation::Merge)],
                alternative: Some(NextStep::Abort(InProgressOperation::Merge)),
            }
        );
    }

    #[test]
    fn steps_without_a_command_fall_back_to_conflict_and_merge_guidance() {
        let unrunnable = Operation {
            kind: InProgressOperation::Am,
            next_steps: vec![NextStep::Continue(InProgressOperation::Am)],
            alternatives: vec![NextStep::Abort(InProgressOperation::Am)],
        };
        let none = standing(None, None);

        let mut m = model();
        m.operation = Some(unrunnable.clone());
        m.conflict_details
            .push(conflict("a", ConflictKind::BothAdded));
        assert_eq!(m.guidance(&none), StatusGuidance::ResolveConflictsThenStage);

        m.state.merge_in_progress = true;
        assert_eq!(
            m.guidance(&none),
            StatusGuidance::ResolveConflictsThenContinueMerge
        );

        m.conflict_details.clear();
        assert_eq!(m.guidance(&none), StatusGuidance::ContinueMerge);

        let mut m = model();
        m.operation = Some(unrunnable);
        m.staged.push(modified("s"));
        assert_eq!(
            m.guidance(&none),
            StatusGuidance::Actions(vec![StatusAction::Commit])
        );
    }

    #[test]
    fn alternatives_without_a_command_are_skipped() {
        let mut m = model();
        m.operation = Some(Operation {
            kind: InProgressOperation::Merge,
            next_steps: vec![
                NextStep::ContinueWithGit(InProgressOperation::Bisect),
                NextStep::Continue(InProgressOperation::Merge),
            ],
            alternatives: vec![
                NextStep::Abort(InProgressOperation::Am),
                NextStep::Abort(InProgressOperation::Merge),
            ],
        });
        assert_eq!(
            m.guidance(&standing(None, None)),
            StatusGuidance::Operation {
                steps: vec![NextStep::Continue(InProgressOperation::Merge)],
                alternative: Some(NextStep::Abort(InProgressOperation::Merge)),
            }
        );
    }
}

#[cfg(test)]
mod pathspec_prune_tests {
    use super::pathspec_may_match_directory;

    /// Directory prune without the literal fast path: every spec also goes through
    /// [`crate::pathspec::pathspec_matches`].
    fn may_match_via_full_matcher(rel_dir: &str, spec: &str) -> bool {
        let rel_dir = rel_dir.trim_end_matches('/');
        let spec_norm = spec.trim_end_matches('/');
        rel_dir.is_empty()
            || crate::pathspec::has_glob_chars(spec)
            || spec_norm == rel_dir
            || spec_norm.starts_with(&format!("{rel_dir}/"))
            || rel_dir.starts_with(&format!("{spec_norm}/"))
            || crate::pathspec::pathspec_matches(spec, rel_dir)
    }

    #[test]
    fn literal_fast_path_agrees_with_full_matcher() {
        let specs = [
            "a",
            "a/",
            "a/b",
            "a/b/c.txt",
            "ab",
            "a/bc",
            "b",
            ".",
            "./a",
            "./a/b",
            "",
            "/",
            ":(icase)A/B",
            ":(literal)a*",
            ":!a",
            ":(glob)a/**",
            "a/*",
            "a?",
            "A/b",
        ];
        let dirs = [
            "",
            "a",
            "a/",
            "a/b",
            "a/b/c.txt",
            "ab",
            "a/bc",
            "b",
            "A",
            "x/a",
        ];
        for spec in specs {
            for dir in dirs {
                assert_eq!(
                    pathspec_may_match_directory(dir, &[spec.to_owned()]),
                    may_match_via_full_matcher(dir, spec),
                    "spec {spec:?} dir {dir:?}"
                );
            }
        }
        let many: Vec<String> = (0..100).map(|i| format!("d{i}/f.txt")).collect();
        assert!(pathspec_may_match_directory("d42", &many));
        assert!(pathspec_may_match_directory("d42/f.txt", &many));
        assert!(!pathspec_may_match_directory("d100", &many));
        assert!(!pathspec_may_match_directory("d42/g", &many));
    }
}

#[cfg(test)]
mod status_op_tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::progress::{NeverCancel, NullProgress};
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

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
    fn unmerged_gitlink_is_conflict_not_untracked() {
        use crate::index::{Index, IndexEntry, MODE_GITLINK};
        use crate::objects::ObjectKind;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        let sub = root.join("vendor/lib");
        fs::create_dir_all(&sub).unwrap();
        let git = sub.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        fs::write(sub.join("f.txt"), b"sub\n").unwrap();
        let sub_repo =
            Repository::open(&git, &RepositoryOptions::empty().with_work_tree(&sub)).unwrap();
        let blob = sub_repo.odb().write(ObjectKind::Blob, b"sub\n").unwrap();
        let tree = sub_repo
            .odb()
            .write(
                ObjectKind::Tree,
                format!("100644 {blob}\tf.txt\n").as_bytes(),
            )
            .unwrap();
        let commit = sub_repo
            .odb()
            .write(
                ObjectKind::Commit,
                format!("tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\nm\n")
                    .as_bytes(),
            )
            .unwrap();
        crate::refs::write_ref(&git, "refs/heads/main", &commit).unwrap();

        let gitlink = commit;
        let mut index = Index::new();
        for stage in 1..=3u8 {
            index.entries.push(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_GITLINK,
                uid: 0,
                gid: 0,
                size: 0,
                oid: gitlink,
                flags: (b"vendor/lib".len() as u16) | ((stage as u16) << 12),
                flags_extended: None,
                path: b"vendor/lib".to_vec(),
                base_index_pos: 0,
            });
        }
        index.sort();
        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        repo.write_index(&mut index).unwrap();

        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(
            model.conflicts.iter().any(|p| p == "vendor/lib"),
            "conflicts: {:?}",
            model.conflicts
        );
        assert!(
            !model.untracked.iter().any(|p| p.starts_with("vendor/lib")),
            "untracked must not list conflicted submodule: {:?}",
            model.untracked
        );
    }

    #[test]
    fn expand_untracked_for_staging_walks_each_collapsed_dir_once() {
        const DIRS: u32 = 200;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        let mut collapsed = Vec::new();
        let mut want = Vec::new();
        for i in 0..DIRS {
            let dir = root.join(format!("d{i}/sub"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("f.txt"), b"x\n").unwrap();
            fs::write(dir.join("f.log"), b"x\n").unwrap();
            collapsed.push(format!("d{i}/"));
            want.push(format!("d{i}/sub/f.txt"));
        }
        fs::create_dir_all(root.join("d0/build")).unwrap();
        fs::write(root.join("d0/build/out.txt"), b"x\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.txt"), b"x\n").unwrap();
        collapsed.push("build/".to_owned());
        init_min_repo(&root.join("d1/nested"));
        want.push("d1/nested/".to_owned());
        want.push("loose.txt".to_owned());
        collapsed.push("loose.txt".to_owned());
        want.sort();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let mut index = repo.load_index().unwrap();
        for pathspecs in [vec![], vec!["*.txt".to_owned()], vec![".".to_owned()]] {
            untracked_walk_probe::reset();
            let expanded = expand_untracked_for_staging(
                &repo,
                &mut index,
                root,
                collapsed.clone(),
                &pathspecs,
            )
            .unwrap();
            let dirs_read = untracked_walk_probe::dirs_read();
            assert_eq!(expanded, want, "{pathspecs:?}");
            assert!(
                dirs_read <= 2 * DIRS + 8,
                "{pathspecs:?}: expanding {DIRS} collapsed directories listed {dirs_read}"
            );
        }

        let limited =
            expand_untracked_for_staging(&repo, &mut index, root, collapsed, &["d7/".to_owned()])
                .unwrap();
        assert_eq!(
            limited,
            vec!["d7/sub/f.txt".to_owned(), "loose.txt".to_owned()],
            "collapsed directories are filtered by pathspec; listed files pass through"
        );
    }

    fn listing_entry(path: &str, stage: u8) -> crate::index::IndexEntry {
        let mut entry = crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::from_bytes(&[0u8; 20]).unwrap(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        };
        entry.set_stage(stage);
        entry
    }

    fn listing_names(listing: &UntrackedListing) -> Vec<&str> {
        listing
            .candidates
            .iter()
            .map(|(name, _)| std::str::from_utf8(name).unwrap())
            .collect()
    }

    #[test]
    fn untracked_listing_drops_only_tracked_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sub");
        for name in ["z.txt", "a.txt", "both.txt", "new.txt", "A.TXT"] {
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), name).unwrap();
        }
        fs::create_dir_all(dir.join("tracked-dir")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        let mut index = Index::new();
        index.entries = vec![
            listing_entry("sub/a.txt", 0),
            listing_entry("sub/both.txt", 2),
            listing_entry("sub/both.txt", 3),
            listing_entry("sub/tracked-dir", 0),
            listing_entry("sub/tracked-dir/x", 0),
            listing_entry("sub/z.txt", 0),
            listing_entry("subz.txt", 0),
        ];
        index.sort();

        let listing =
            untracked_candidates_sorted(fs::read_dir(&dir).unwrap(), &index, "sub", false);
        assert_eq!(
            listing_names(&listing),
            vec![".git", "A.TXT", "new.txt", "tracked-dir"],
            "tracked files (any stage) are dropped; directories, case variants and \
             untracked files stay, ordered by name bytes"
        );
        assert!(listing.has_dot_git);

        index.mark_entries_unsorted();
        let unsorted =
            untracked_candidates_sorted(fs::read_dir(&dir).unwrap(), &index, "sub", false);
        assert_eq!(
            listing_names(&unsorted),
            vec![
                ".git",
                "A.TXT",
                "a.txt",
                "both.txt",
                "new.txt",
                "tracked-dir",
                "z.txt"
            ],
            "an index not known to be sorted keeps every entry for the full check"
        );
    }

    #[cfg(unix)]
    #[test]
    fn untracked_listing_matches_raw_bytes_of_a_non_utf8_directory() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().unwrap();
        let raw_dir: &[u8] = b"raw-\xff";
        let dir = tmp.path().join(std::ffi::OsStr::from_bytes(raw_dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("t.txt"), b"x").unwrap();
        fs::write(dir.join("u.txt"), b"x").unwrap();
        let mut index = Index::new();
        let mut tracked = listing_entry("t.txt", 0);
        tracked.path = [raw_dir, b"/t.txt"].concat();
        index.entries = vec![tracked];
        index.sort();

        let rel = crate::repo_relpath::storage_string(raw_dir);
        assert_ne!(rel.as_bytes(), raw_dir, "the directory is stored encoded");
        let listing = untracked_candidates_sorted(fs::read_dir(&dir).unwrap(), &index, &rel, false);
        assert_eq!(listing_names(&listing), vec!["u.txt"]);
    }

    #[test]
    fn untracked_listing_at_root_and_with_precompose() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("top.txt"), b"x").unwrap();
        fs::write(root.join("caf\u{e9}.txt"), b"x").unwrap();
        fs::write(root.join("new.txt"), b"x").unwrap();
        let mut index = Index::new();
        index.entries = vec![
            listing_entry("caf\u{e9}.txt", 0),
            listing_entry("top.txt", 0),
        ];
        index.sort();

        let plain = untracked_candidates_sorted(fs::read_dir(root).unwrap(), &index, "", false);
        assert_eq!(listing_names(&plain), vec!["new.txt"]);
        assert!(!plain.has_dot_git);

        let precompose = untracked_candidates_sorted(fs::read_dir(root).unwrap(), &index, "", true);
        assert_eq!(
            listing_names(&precompose),
            vec!["caf\u{e9}.txt", "new.txt"],
            "non-ASCII names are left to the precomposing check"
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

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
    fn status_ignored_collapses_directory_exclude_without_full_walk() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_min_repo(root);
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        let nm = root.join("node_modules");
        fs::create_dir_all(nm.join("p0/m0")).unwrap();
        for i in 0..500 {
            fs::write(nm.join(format!("p0/m0/f{i}.txt")), b"x\n").unwrap();
        }

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        untracked_walk_probe::reset();
        let model = status(
            &repo,
            &StatusOptions {
                ignored: IgnoredMode::Traditional,
                ..StatusOptions::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        assert_eq!(
            model.ignored,
            vec!["node_modules/".to_owned()],
            "directory exclude must collapse to a single ignored path"
        );
        let dirs_read = untracked_walk_probe::dirs_read();
        assert!(
            dirs_read <= 4,
            "ignored directory exclude must not read every nested directory \
             (listed {dirs_read} directories)"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let _ = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        let tracked = repo.load_index().unwrap().entries.len();
        let dir_count = (FILE_COUNT + 99) / 100;
        let calls = worktree_metadata_probe_count();
        assert!(
            calls <= tracked + dir_count + 5,
            "status scan metadata calls {calls} exceeded budget {} (tracked {tracked}, dirs {dir_count})",
            tracked + dir_count + 5
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
        assert!(
            model.untracked.is_empty(),
            "untracked=No must report nothing, got {:?}",
            model.untracked
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        untracked_walk_probe::reset();
        let _ = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        let entries_read = untracked_walk_probe::count();
        assert!(
            entries_read <= 32,
            "populated UNTR must answer warm status without a full worktree rescan \
             (read {entries_read} directory entries)"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn operation_next_steps_merge() {
        use crate::state::{operation_next_steps, InProgressOperation, NextStep};

        let tmp = merge_conflict_repo_for_tests();
        let root = tmp.path();
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();

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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
            let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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
        let model_both = status(&repo, &opts_both, &mut NullProgress, &NeverCancel).unwrap();
        assert!(
            model_both
                .staged
                .iter()
                .any(|e| e.status == crate::diff::DiffStatus::Renamed),
            "full tree should show rename: {:?}",
            model_both.staged
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn status_unstaged_modified_uses_null_worktree_oid_on_size_drift() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a"), b"a\n").unwrap();
        grit_test_support::git(root, &["add", "a"]);
        grit_test_support::git(root, &["commit", "-qm", "c"]);
        let index_oid = {
            use std::str::FromStr;
            crate::objects::ObjectId::from_str(
                grit_test_support::git(root, &["rev-parse", "HEAD:a"]).trim(),
            )
            .unwrap()
        };
        let mut content = fs::read(root.join("a")).unwrap();
        content.extend_from_slice(b"more");
        fs::write(root.join("a"), content).unwrap();

        let git_raw = grit_test_support::git(root, &["diff-files", "--raw"]);
        assert!(
            git_raw.contains(&format!(
                "{index_oid} 0000000000000000000000000000000000000000"
            )),
            "git diff-files --raw must use null worktree oid: {git_raw}"
        );

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .unwrap();
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert_eq!(model.unstaged.len(), 1, "{:?}", model.unstaged);
        let e = &model.unstaged[0];
        assert_eq!(e.status, crate::diff::DiffStatus::Modified);
        assert_eq!(e.path(), "a");
        assert_eq!(e.old_oid, index_oid);
        assert_eq!(e.new_oid, crate::diff::zero_oid());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
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
        status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        grit_test_support::git(root, &["status"]);

        untracked_walk_probe::reset();
        let _ = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        let entries_read = untracked_walk_probe::count();
        assert!(
            entries_read <= 32,
            "after system git status, grit must reuse UNTR without a full rescan \
             (read {entries_read} directory entries)"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let model = status(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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

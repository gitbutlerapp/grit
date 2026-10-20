//! Diff target selection and [`FileDiff`] assembly for porcelain and the CLI.

use std::path::Path;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::config::ConfigSet;
use crate::diff::diff_trees_with_renames_and_policy;
use crate::diff::structured::{
    structured_file_diff, SideBytes, StructuredFileDiff, StructuredHunk,
};
use crate::diff::{
    apply_rename_detection_to_diff_entries, diff_commits_with_renames_and_policy,
    diff_index_to_tree_with_renames, diff_index_to_worktree_with_renames,
    diff_tree_to_worktree_with_git_dir_and_rules, index_mtime_for_diff, read_submodule_head_oid,
    read_worktree_side_bytes_for_diff, submodule_porcelain_flags, tree_diff_structured_parallel,
    DiffEntry, DiffIndexToWorktreeOptions, DiffOptions, DiffStatus, TreeDiffStructured,
};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::objects::odb::Odb;
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::parallel::{par_map, ParallelOp, ThreadPolicy};
use crate::porcelain::pathspec_match::{
    ensure_pathspec_matches_known_in_repository, pathspec_matches_known_in_repository,
};
use crate::repo::Repository;
use crate::revwalk::merge_base::merge_base_for_diff_two_commits;
use crate::revwalk::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_double_dot_range, split_triple_dot_range,
};
use crate::state::resolve_head;
use crate::worktree::fsmonitor::fsmonitor_enabled;
use crate::worktree::pathspec::{resolve_pathspec, resolve_pathspec_in_worktree_with_git_dir};
use crate::worktree::worktree_rules::WorktreeRules;

/// What to compare when producing a structured file diff list.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// Working tree and index together vs `HEAD`'s tree (default uncommitted diff).
    WorktreeVsHead,
    /// Working tree (and index) vs a specific tree OID.
    WorktreeVsTree(ObjectId),
    /// Index vs `HEAD`'s tree (`--staged`).
    IndexVsHead,
    /// Working tree vs index (`--unstaged`).
    WorktreeVsIndex,
    /// Patch introduced by one commit (parent tree vs commit tree).
    Commit(ObjectId),
    /// Tree diff between two commits (`old` may be absent for root commits).
    Trees {
        /// Older commit (left side of the range).
        old: Option<ObjectId>,
        /// Newer commit (right side).
        new: ObjectId,
    },
}

/// One changed file with structured hunks and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Display path (new path, or old path for a deletion).
    pub path: String,
    /// Pre-rename path when it differs from [`Self::path`].
    pub old_path: Option<String>,
    /// Change kind for this path.
    pub status: DiffStatus,
    /// Old git mode (octal), when present on this side.
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    pub new_mode: Option<String>,
    /// Old blob OID on this side (zero when absent).
    pub old_oid: ObjectId,
    /// New blob OID on this side (zero when absent).
    pub new_oid: ObjectId,
    /// True when either side was treated as binary (NUL in the first 8 KiB).
    pub binary: bool,
    /// True when blob content changed (not a mode-only touch on the same OID).
    pub content_changed: bool,
    /// Old side was decoded with lossy UTF-8.
    pub old_encoding_lossy: bool,
    /// New side was decoded with lossy UTF-8.
    pub new_encoding_lossy: bool,
    /// Structured line hunks for text files.
    pub hunks: Vec<StructuredHunk>,
}

/// Resolved diff mode and normalized pathspecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffArguments {
    /// What to diff.
    pub target: DiffTarget,
    /// Repository-relative pathspecs (empty = all paths).
    pub pathspecs: Vec<String>,
}

/// Invalid `grit diff` argument combinations or revision parsing failures.
#[non_exhaustive]
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResolveDiffArgumentsError {
    /// `--staged` and `--unstaged` together.
    #[error("--staged and --unstaged cannot be combined")]
    StagedAndUnstaged,

    /// `--staged` with commit arguments or `--against`.
    #[error("--staged cannot be combined with revision arguments or --against")]
    StagedWithRevisions,

    /// `--unstaged` with commit arguments or `--against`.
    #[error("--unstaged cannot be combined with revision arguments or --against")]
    UnstagedWithRevisions,

    /// More than two commit-ish positionals (excluding range syntax in one token).
    #[error("at most two commits, or one two-dot range such as A..B")]
    TooManyRevisions,

    /// `--against` with commit positionals.
    #[error("--against compares the worktree to a ref and takes no commit arguments")]
    AgainstWithCommits,

    /// Second positional after a range token should be passed after `--`.
    #[error(
        "a two-dot or three-dot range uses the other positional as a path limit; pass paths after `--`"
    )]
    RangeWithSecondRevision,

    /// Underlying revision or pathspec resolution failure.
    #[error("{0}")]
    Message(String),
}

impl From<Error> for ResolveDiffArgumentsError {
    fn from(err: Error) -> Self {
        match err {
            Error::InvalidRef(msg) | Error::Message(msg) => Self::Message(msg),
            other => Self::Message(other.to_string()),
        }
    }
}

/// Parse `grit diff` flags and positionals into a [`DiffTarget`] and pathspecs.
///
/// `cwd_prefix` is the path from the process cwd to the work tree root (from [`pathdiff`](crate::worktree::pathspec::pathdiff)); pass `None` when unknown or in a bare repo.
///
/// # Errors
///
/// Returns [`ResolveDiffArgumentsError`] for flag conflicts or unresolvable revisions.
pub fn resolve_diff_arguments(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    staged: bool,
    unstaged: bool,
    against: Option<&str>,
    positionals: &[String],
    explicit_paths: &[String],
) -> std::result::Result<DiffArguments, ResolveDiffArgumentsError> {
    let (commits, path_args) = disambiguate_diff_revision_and_paths(
        repo,
        cwd_prefix,
        positionals.to_vec(),
        explicit_paths.to_vec(),
    );

    if staged && unstaged {
        return Err(ResolveDiffArgumentsError::StagedAndUnstaged);
    }
    if staged {
        if !commits.is_empty() || against.is_some() {
            return Err(ResolveDiffArgumentsError::StagedWithRevisions);
        }
        let pathspecs = resolve_worktree_diff_pathspecs(repo, cwd_prefix, &path_args)?;
        return Ok(DiffArguments {
            target: DiffTarget::IndexVsHead,
            pathspecs,
        });
    }
    if unstaged {
        if !commits.is_empty() || against.is_some() {
            return Err(ResolveDiffArgumentsError::UnstagedWithRevisions);
        }
        let pathspecs = resolve_worktree_diff_pathspecs(repo, cwd_prefix, &path_args)?;
        return Ok(DiffArguments {
            target: DiffTarget::WorktreeVsIndex,
            pathspecs,
        });
    }
    if commits.len() > 2 {
        return Err(ResolveDiffArgumentsError::TooManyRevisions);
    }
    if against.is_some() && !commits.is_empty() {
        return Err(ResolveDiffArgumentsError::AgainstWithCommits);
    }

    match commits.len() {
        0 => {
            let pathspecs = resolve_worktree_diff_pathspecs(repo, cwd_prefix, &path_args)?;
            let target = if let Some(spec) = against {
                let commit = resolve_diff_commit(repo, spec)?;
                let tree = commit_tree(repo, &commit)?;
                DiffTarget::WorktreeVsTree(tree)
            } else {
                DiffTarget::WorktreeVsHead
            };
            Ok(DiffArguments { target, pathspecs })
        }
        1 => {
            let spec = &commits[0];
            if let Some((base, tip)) = parse_commit_range(repo, spec)? {
                let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_args)?;
                return Ok(DiffArguments {
                    target: DiffTarget::Trees {
                        old: Some(base),
                        new: tip,
                    },
                    pathspecs,
                });
            }
            if let Ok(oid) = resolve_diff_commit(repo, spec) {
                let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_args)?;
                return Ok(DiffArguments {
                    target: DiffTarget::Commit(oid),
                    pathspecs,
                });
            }
            if path_args.is_empty() {
                let pathspecs =
                    resolve_worktree_diff_pathspecs(repo, cwd_prefix, std::slice::from_ref(spec))?;
                Ok(DiffArguments {
                    target: DiffTarget::WorktreeVsIndex,
                    pathspecs,
                })
            } else if let Err(e) = resolve_diff_commit(repo, spec) {
                Err(e.into())
            } else {
                Err(ResolveDiffArgumentsError::Message(
                    "revision unresolved".into(),
                ))
            }
        }
        2 => {
            if let Some((base, tip)) = parse_commit_range(repo, &commits[0])? {
                if token_is_known_worktree_pathspec(repo, cwd_prefix, &commits[1]) {
                    let pathspecs = resolve_commit_diff_pathspecs(
                        repo,
                        cwd_prefix,
                        std::slice::from_ref(&commits[1]),
                    )?;
                    return Ok(DiffArguments {
                        target: DiffTarget::Trees {
                            old: Some(base),
                            new: tip,
                        },
                        pathspecs,
                    });
                }
                if resolve_diff_commit(repo, &commits[1]).is_ok() {
                    return Err(ResolveDiffArgumentsError::RangeWithSecondRevision);
                }
                if let Err(e) = resolve_diff_commit(repo, &commits[1]) {
                    return Err(e.into());
                }
                return Err(ResolveDiffArgumentsError::Message(
                    "could not resolve second revision".into(),
                ));
            }
            let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_args)?;
            let base = resolve_diff_commit(repo, &commits[0])?;
            let tip = resolve_diff_commit(repo, &commits[1])?;
            Ok(DiffArguments {
                target: DiffTarget::Trees {
                    old: Some(base),
                    new: tip,
                },
                pathspecs,
            })
        }
        _ => unreachable!("checked above"),
    }
}

/// Build [`FileDiff`] rows from tree diff entries without a [`DiffTarget`].
pub(crate) fn file_diffs_from_entries(
    repo: &Repository,
    mut entries: Vec<DiffEntry>,
    opts: DiffOptions,
) -> Result<Vec<FileDiff>> {
    sort_entries(&mut entries);
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    let structured =
        tree_diff_structured_parallel(repo.odb(), entries, &config, &thread_policy, opts)?;
    Ok(structured
        .into_iter()
        .map(file_diff_from_tree_structured)
        .collect())
}

pub(crate) fn collect_files_for_target(
    repo: &Repository,
    target: &DiffTarget,
    pathspecs: &[String],
    opts: DiffOptions,
) -> Result<Vec<FileDiff>> {
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    // Git uses similarity 50 by default; when renames are off, skip detection (101 = never).
    let rename_threshold = opts.renames.threshold_percent().unwrap_or(101);

    let files = match target {
        DiffTarget::WorktreeVsHead => {
            let head = resolve_head(repo.git_dir())?;
            let head_tree = match head.oid() {
                Some(oid) => Some(commit_tree(repo, oid)?),
                None => None,
            };
            let changes = filter_file_changes(
                worktree_changes(repo, head_tree.as_ref(), rename_threshold)?,
                repo,
                pathspecs,
            );
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts)?
        }
        DiffTarget::WorktreeVsTree(tree) => {
            let changes = filter_file_changes(
                worktree_changes(repo, Some(tree), rename_threshold)?,
                repo,
                pathspecs,
            );
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts)?
        }
        DiffTarget::IndexVsHead => {
            let changes =
                filter_file_changes(staged_changes(repo, rename_threshold)?, repo, pathspecs);
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts)?
        }
        DiffTarget::WorktreeVsIndex => {
            let changes =
                filter_file_changes(unstaged_changes(repo, rename_threshold)?, repo, pathspecs);
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts)?
        }
        DiffTarget::Commit(oid) => {
            let items = commit_changes(repo, oid, &config, &thread_policy, opts, rename_threshold)?;
            filter_and_map_tree_structured(items, repo, pathspecs)
        }
        DiffTarget::Trees { old, new } => {
            let items = if let Some(base) = old {
                tree_commit_changes(
                    repo,
                    base,
                    new,
                    &config,
                    &thread_policy,
                    opts,
                    rename_threshold,
                )?
            } else {
                commit_changes(repo, new, &config, &thread_policy, opts, rename_threshold)?
            };
            filter_and_map_tree_structured(items, repo, pathspecs)
        }
    };
    Ok(files)
}

fn filter_and_map_tree_structured(
    items: Vec<TreeDiffStructured>,
    repo: &Repository,
    pathspecs: &[String],
) -> Vec<FileDiff> {
    filter_tree_diff(items, repo, pathspecs)
        .into_iter()
        .map(file_diff_from_tree_structured)
        .collect()
}

fn file_diff_from_tree_structured(t: TreeDiffStructured) -> FileDiff {
    let e = t.entry;
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    let old_path = e.old_path.filter(|op| *op != path);
    FileDiff {
        path,
        old_path,
        status: e.status,
        old_mode: t.structured.old_mode,
        new_mode: t.structured.new_mode,
        old_oid: e.old_oid,
        new_oid: e.new_oid,
        binary: t.binary,
        content_changed: e.old_oid != e.new_oid,
        old_encoding_lossy: t.structured.old_encoding_lossy,
        new_encoding_lossy: t.structured.new_encoding_lossy,
        hunks: t.structured.hunks,
    }
}

struct WorktreeDiffSide<'a> {
    work_tree: &'a Path,
    index: &'a Index,
    rules: Arc<Mutex<WorktreeRules>>,
}

#[derive(Clone)]
struct FileChange {
    path: String,
    old_path: Option<String>,
    status: DiffStatus,
    old_mode: String,
    new_mode: String,
    old_oid: ObjectId,
    new_oid: ObjectId,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
    content_changed: bool,
}

fn file_diffs_from_worktree_changes(
    changes: Vec<FileChange>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    opts: DiffOptions,
) -> Result<Vec<FileDiff>> {
    let opts = opts.cap_word_level_inline_for_changed_files(changes.len());
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = 0;
    let est_bytes = changes.len().saturating_mul(8192);
    Ok(par_map(&changes, par_opts, est_bytes, |fc| {
        build_file_diff(fc, opts)
    }))
}

fn build_file_diff(fc: &FileChange, diff_options: DiffOptions) -> FileDiff {
    let structured = structured_for_file_change(fc, diff_options);
    FileDiff {
        path: fc.path.clone(),
        old_path: fc.old_path.clone(),
        status: fc.status,
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_oid: fc.old_oid,
        new_oid: fc.new_oid,
        binary: fc.binary,
        content_changed: fc.content_changed,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        hunks: structured.hunks,
    }
}

fn structured_for_file_change(fc: &FileChange, diff_options: DiffOptions) -> StructuredFileDiff {
    if fc.binary {
        StructuredFileDiff {
            old_mode: diff_mode_display(&fc.old_mode),
            new_mode: diff_mode_display(&fc.new_mode),
            old_encoding_lossy: fc.old_bytes.encoding_lossy,
            new_encoding_lossy: fc.new_bytes.encoding_lossy,
            hunks: Vec::new(),
        }
    } else {
        structured_file_diff(
            &fc.old_mode,
            &fc.new_mode,
            &fc.old_bytes,
            &fc.new_bytes,
            diff_options,
        )
    }
}

fn diff_mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

fn token_is_diff_revision(repo: &Repository, token: &str) -> bool {
    resolve_diff_commit(repo, token).is_ok()
        || parse_commit_range(repo, token).ok().flatten().is_some()
}

fn token_is_known_worktree_pathspec(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    token: &str,
) -> bool {
    let Some(work_tree) = repo.work_tree() else {
        return false;
    };
    let Ok(resolved) = resolve_pathspec_in_worktree_with_git_dir(
        token,
        token,
        work_tree,
        cwd_prefix,
        Some(repo.git_dir()),
    ) else {
        return false;
    };
    pathspec_matches_known_in_repository(repo, &resolved).unwrap_or(false)
}

fn disambiguate_diff_revision_and_paths(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    mut commits: Vec<String>,
    mut paths: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    if paths.is_empty() && commits.len() == 2 {
        let first_rev = token_is_diff_revision(repo, &commits[0]);
        let second_rev = token_is_diff_revision(repo, &commits[1]);
        if first_rev
            && !second_rev
            && token_is_known_worktree_pathspec(repo, cwd_prefix, &commits[1])
        {
            if let Some(path) = commits.pop() {
                paths.push(path);
            }
        }
    } else if paths.is_empty() {
        while commits.len() >= 2 {
            let Some(tail) = commits.last() else {
                break;
            };
            if token_is_diff_revision(repo, tail) {
                break;
            }
            let Some(path) = commits.pop() else {
                break;
            };
            paths.push(path);
        }
    }
    (commits, paths)
}

fn pathspec_resolution_root(repo: &Repository) -> &Path {
    repo.work_tree().unwrap_or_else(|| repo.git_dir())
}

fn resolve_worktree_diff_pathspecs(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    raw_paths: &[String],
) -> std::result::Result<Vec<String>, ResolveDiffArgumentsError> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let work_tree = repo.work_tree().ok_or_else(|| {
        ResolveDiffArgumentsError::Message("grit diff needs a working tree".into())
    })?;
    let mut specs = Vec::with_capacity(raw_paths.len());
    for pathspec in raw_paths {
        let resolved = resolve_pathspec_in_worktree_with_git_dir(
            pathspec,
            pathspec,
            work_tree,
            cwd_prefix,
            Some(repo.git_dir()),
        )
        .map_err(|e| ResolveDiffArgumentsError::Message(e.to_string()))?;
        ensure_pathspec_matches_known_in_repository(repo, pathspec, &resolved)
            .map_err(|e| ResolveDiffArgumentsError::Message(e.to_string()))?;
        specs.push(resolved);
    }
    Ok(specs)
}

fn resolve_commit_diff_pathspecs(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    raw_paths: &[String],
) -> std::result::Result<Vec<String>, ResolveDiffArgumentsError> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let root = pathspec_resolution_root(repo);
    let mut specs = Vec::with_capacity(raw_paths.len());
    for pathspec in raw_paths {
        if let Some(work_tree) = repo.work_tree() {
            let resolved = resolve_pathspec_in_worktree_with_git_dir(
                pathspec,
                pathspec,
                work_tree,
                cwd_prefix,
                Some(repo.git_dir()),
            )
            .map_err(|e| ResolveDiffArgumentsError::Message(e.to_string()))?;
            specs.push(resolved);
        } else {
            specs.push(resolve_pathspec(pathspec, root, cwd_prefix));
        }
    }
    Ok(specs)
}

fn file_change_matches_pathspecs(
    repo: &Repository,
    path: &str,
    old_path: Option<&str>,
    specs: &[String],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    if repo.paths_match_pathspecs(path, specs) {
        return true;
    }
    if let Some(op) = old_path {
        if op != path && repo.paths_match_pathspecs(op, specs) {
            return true;
        }
    }
    false
}

fn filter_file_changes(
    changes: Vec<FileChange>,
    repo: &Repository,
    specs: &[String],
) -> Vec<FileChange> {
    if specs.is_empty() {
        return changes;
    }
    changes
        .into_iter()
        .filter(|c| file_change_matches_pathspecs(repo, &c.path, c.old_path.as_deref(), specs))
        .collect()
}

fn filter_tree_diff(
    items: Vec<TreeDiffStructured>,
    repo: &Repository,
    specs: &[String],
) -> Vec<TreeDiffStructured> {
    if specs.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|t| {
            let e = &t.entry;
            let path = e
                .new_path
                .as_deref()
                .or(e.old_path.as_deref())
                .unwrap_or("");
            let old_path = e.old_path.as_deref();
            file_change_matches_pathspecs(repo, path, old_path, specs)
        })
        .collect()
}

fn unstaged_changes(repo: &Repository, rename_threshold: u32) -> Result<Vec<FileChange>> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("grit diff needs a working tree".into()))?;
    let mut index = repo.index_handle().load()?;
    let rules_arc = Arc::new(Mutex::new(WorktreeRules::from_repository(repo, &index)?));
    let config_arc = rules_arc
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .config_arc();
    let diff_index_mtime = index_mtime_for_diff(&index, None);
    let diff_opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        index_mtime: diff_index_mtime,
        config: Some(config_arc.clone()),
        thread_policy: Some(repo.thread_policy()),
        refresh_index_stat_in_pass: false,
        for_status: true,
        refresh_fsmonitor: fsmonitor_enabled(config_arc.as_ref()),
        worktree_rules: Some(Arc::clone(&rules_arc)),
        command_runner: Some(repo.command_runner()),
        ..DiffIndexToWorktreeOptions::default()
    };
    let (mut entries, _) = diff_index_to_worktree_with_renames(
        repo,
        &mut index,
        work_tree,
        diff_opts,
        rename_threshold,
    )?;
    sort_entries(&mut entries);
    let side = WorktreeDiffSide {
        work_tree,
        index: &index,
        rules: Arc::clone(&rules_arc),
    };
    entries_to_file_changes(repo, entries, Some(&side))
}

fn staged_changes(repo: &Repository, rename_threshold: u32) -> Result<Vec<FileChange>> {
    let head = resolve_head(repo.git_dir())?;
    let head_tree = match head.oid() {
        Some(oid) => Some(commit_tree(repo, oid)?),
        None => None,
    };
    let index = repo.index_handle().load()?;
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    let mut entries = diff_index_to_tree_with_renames(
        repo.odb(),
        &index,
        head_tree.as_ref(),
        false,
        rename_threshold,
        config.as_ref(),
        &thread_policy,
    )?;
    sort_entries(&mut entries);
    entries_to_file_changes(repo, entries, None)
}

fn worktree_changes(
    repo: &Repository,
    base_tree: Option<&ObjectId>,
    rename_threshold: u32,
) -> Result<Vec<FileChange>> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("grit diff needs a working tree".into()))?;
    let mut index = repo.index_handle().load()?;
    let config = repo.config()?;

    let index_backed = base_tree
        .zip(index.cache_tree_root.as_ref())
        .map(|(left, right)| left == right)
        .unwrap_or(base_tree.is_none() && index.cache_tree_root.is_none())
        && crate::index::write_tree::cache_tree_extension_valid(index.cache_tree.as_ref());
    let diff_index_mtime = index_mtime_for_diff(&index, None);
    let rules_arc = Arc::new(Mutex::new(WorktreeRules::from_repository(repo, &index)?));
    let mut entries = if index_backed {
        let (entries, _) = diff_index_to_worktree_with_renames(
            repo,
            &mut index,
            work_tree,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                config: Some(config.clone()),
                index_mtime: diff_index_mtime,
                thread_policy: Some(repo.thread_policy()),
                refresh_index_stat_in_pass: false,
                for_status: true,
                refresh_fsmonitor: fsmonitor_enabled(config.as_ref()),
                worktree_rules: Some(Arc::clone(&rules_arc)),
                command_runner: Some(repo.command_runner()),
                ..DiffIndexToWorktreeOptions::default()
            },
            rename_threshold,
        )?;
        entries
    } else {
        let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
        let raw = diff_tree_to_worktree_with_git_dir_and_rules(
            repo.odb(),
            base_tree,
            work_tree,
            repo.git_dir(),
            &index,
            Some(config.as_ref()),
            Some(&worktree_rules),
        )?;
        apply_rename_detection_to_diff_entries(
            repo.odb(),
            raw,
            Some(work_tree),
            rename_threshold,
            config.as_ref(),
            &repo.thread_policy(),
        )?
    };
    sort_entries(&mut entries);
    let side = WorktreeDiffSide {
        work_tree,
        index: &index,
        rules: rules_arc,
    };
    entries_to_file_changes(repo, entries, Some(&side))
}

fn entries_to_file_changes(
    repo: &Repository,
    entries: Vec<DiffEntry>,
    side: Option<&WorktreeDiffSide<'_>>,
) -> Result<Vec<FileChange>> {
    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(repo.odb(), &e)?;
            let (new_bytes, new_bin) = new_side_bytes(repo.odb(), &e, side)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

fn tree_commit_changes(
    repo: &Repository,
    base: &ObjectId,
    tip: &ObjectId,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    diff_options: DiffOptions,
    rename_threshold: u32,
) -> Result<Vec<TreeDiffStructured>> {
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        base,
        tip,
        rename_threshold,
        config,
        thread_policy,
    )?;
    sort_entries(&mut entries);
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
}

fn commit_changes(
    repo: &Repository,
    oid: &ObjectId,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    diff_options: DiffOptions,
    rename_threshold: u32,
) -> Result<Vec<TreeDiffStructured>> {
    let commit = read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(commit_tree(repo, parent)?),
        None => None,
    };

    let mut entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        old_tree.as_ref(),
        Some(&new_tree),
        "",
        rename_threshold,
        config,
        thread_policy,
    )?;
    sort_entries(&mut entries);
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
}

fn resolve_diff_commit(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision(repo, spec)
        .map_err(|_| Error::Message(format!("could not resolve '{spec}'")))?;
    peel_to_commit_for_merge_base(repo, oid)
}

fn resolve_diff_range_end(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision_for_range_end(repo, spec)?;
    peel_to_commit_for_merge_base(repo, oid)
}

fn parse_commit_range(repo: &Repository, spec: &str) -> Result<Option<(ObjectId, ObjectId)>> {
    if let Some((left, right)) = split_triple_dot_range(spec) {
        let left_c = if left.is_empty() {
            resolve_diff_range_end(repo, "HEAD")?
        } else {
            resolve_diff_range_end(repo, left)?
        };
        let right_c = if right.is_empty() {
            resolve_diff_range_end(repo, "HEAD")?
        } else {
            resolve_diff_range_end(repo, right)?
        };
        let base = merge_base_for_diff_two_commits(repo, left_c, right_c).map_err(|e| {
            Error::Message(match e {
                crate::revwalk::merge_base::MergeBaseForDiffError::None => {
                    "no merge base for that revision range".to_owned()
                }
                crate::revwalk::merge_base::MergeBaseForDiffError::Multiple => {
                    "multiple merge bases for that revision range".to_owned()
                }
                crate::revwalk::merge_base::MergeBaseForDiffError::Other(msg) => msg,
            })
        })?;
        return Ok(Some((base, right_c)));
    }
    let Some((left, right)) = split_double_dot_range(spec) else {
        return Ok(None);
    };
    let base = if left.is_empty() {
        resolve_diff_range_end(repo, "HEAD")?
    } else {
        resolve_diff_range_end(repo, left)?
    };
    let tip = if right.is_empty() {
        resolve_diff_range_end(repo, "HEAD")?
    } else {
        resolve_diff_range_end(repo, right)?
    };
    Ok(Some((base, tip)))
}

fn read_commit(repo: &Repository, oid: &ObjectId) -> Result<CommitData> {
    let object = repo.odb().read(oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::Message(format!(
            "object {oid} is a {}, not a commit",
            object.kind
        )));
    }
    parse_commit(&object.data)
}

fn commit_tree(repo: &Repository, commit: &ObjectId) -> Result<ObjectId> {
    Ok(read_commit(repo, commit)?.tree)
}

fn sort_entries(entries: &mut [DiffEntry]) {
    entries.sort_by(|a, b| a.path().cmp(b.path()));
}

fn file_change(
    e: DiffEntry,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
) -> FileChange {
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    let old_path = e.old_path.filter(|op| *op != path);
    FileChange {
        path,
        old_path,
        status: e.status,
        old_mode: e.old_mode,
        new_mode: e.new_mode,
        old_oid: e.old_oid,
        new_oid: e.new_oid,
        old_bytes,
        new_bytes,
        binary,
        content_changed: e.old_oid != e.new_oid,
    }
}

fn old_side_bytes(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.old_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid, None).into_bytes()),
            false,
        ));
    }
    if e.old_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.old_oid)
}

fn new_side_bytes(
    odb: &Odb,
    e: &DiffEntry,
    work_tree: Option<&WorktreeDiffSide<'_>>,
) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        let wt = work_tree.map(|s| s.work_tree);
        return Ok((
            SideBytes::from_bytes(gitlink_new_line(e, wt).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    if let (Some(ctx), Some(p)) = (work_tree, e.new_path.as_deref().or(e.old_path.as_deref())) {
        let mode = u32::from_str_radix(&e.new_mode, 8).unwrap_or(0);
        let index_entry = ctx.index.get(p.as_bytes(), 0);
        let rules = ctx.rules.lock().unwrap_or_else(|e| e.into_inner());
        let data = read_worktree_side_bytes_for_diff(
            odb,
            ctx.work_tree,
            p,
            mode,
            rules.config(),
            index_entry,
            Some(&rules),
        )?;
        Ok(bytes_for_diff(&data))
    } else {
        blob_bytes(odb, &e.new_oid)
    }
}

fn gitlink_line(oid: &ObjectId, dirty_suffix: Option<&str>) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    let suffix = dirty_suffix.unwrap_or("");
    format!("Subproject commit {}{suffix}\n", oid.to_hex())
}

fn gitlink_new_line(e: &DiffEntry, work_tree: Option<&Path>) -> String {
    let path = e.new_path.as_deref().or(e.old_path.as_deref());
    let resolved = if e.new_oid != ObjectId::zero() {
        e.new_oid
    } else if let (Some(wt), Some(p)) = (work_tree, path) {
        read_submodule_head_oid(&wt.join(p)).unwrap_or(ObjectId::zero())
    } else {
        ObjectId::zero()
    };
    if resolved == ObjectId::zero() {
        return String::new();
    }
    let dirty = e.old_mode == "160000"
        && e.new_mode == "160000"
        && work_tree.is_some_and(|wt| {
            path.is_some_and(|p| submodule_porcelain_flags(wt, p, e.old_oid).modified)
        });
    gitlink_line(&resolved, if dirty { Some("-dirty") } else { None })
}

fn blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(oid)?.data;
    Ok(bytes_for_diff(&data))
}

fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (SideBytes::from_bytes(Vec::new()), true)
    } else {
        (SideBytes::from_bytes(data.to_vec()), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_errors_staged_and_unstaged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::repo::Repository::init(dir.path(), &crate::repo::InitOptions::default())
            .expect("init");
        let err = resolve_diff_arguments(&repo, None, true, true, None, &[], &[]).unwrap_err();
        assert_eq!(err, ResolveDiffArgumentsError::StagedAndUnstaged);
    }
}

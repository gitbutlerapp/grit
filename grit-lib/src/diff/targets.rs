//! Diff target selection and [`FileDiff`] assembly for porcelain and the CLI.

use std::collections::BTreeSet;
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
use crate::error::{Error, PathspecResolveError, Result};
use crate::index::Index;
use crate::index::MODE_TREE;
use crate::objects::odb::Odb;
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::parallel::{par_map, ParallelOp, ThreadPolicy};
use crate::pathspec::{exact_literal_pathspec_set, PathspecMatchContext, PathspecMatcher};
use crate::porcelain::pathspec_match::{
    ensure_pathspec_matches_known_in_repository, pathspec_matches_known_in_repository,
};
use crate::repo::Repository;
use crate::rev_parse::peel_tags_to_commit;
use crate::revwalk::merge_base::{merge_base_for_diff_two_commits, MergeBaseForDiffError};
use crate::revwalk::rev_parse::{
    resolve_revision, resolve_revision_for_range_end, split_double_dot_range,
    split_triple_dot_range,
};
use crate::state::resolve_head;
use crate::worktree::fsmonitor::fsmonitor_enabled;
use crate::worktree::pathspec::resolve_pathspec;
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

/// Unparsed `grit diff` flags and operands, as the user typed them.
///
/// [`resolve_diff_request`] turns this into a [`DiffArguments`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffRequest {
    /// Compare the index with `HEAD` (`--staged`).
    pub staged: bool,
    /// Compare the working tree with the index (`--unstaged`).
    pub unstaged: bool,
    /// Compare the working tree with this revision instead of `HEAD` (`--against`).
    pub against: Option<String>,
    /// Operands before `--`: revisions, ranges, or paths given without `--`.
    pub positionals: Vec<String>,
    /// Paths after `--`.
    pub paths: Vec<String>,
    /// Pathspecs read from a file (`--pathspec-from-file`), placed before the path operands.
    ///
    /// `Some` means a file was supplied, even when it held no entries: the combined pathspec
    /// list must then be non-empty.
    pub pathspec_file: Option<Vec<String>>,
}

/// Invalid `grit diff` argument combinations or revision parsing failures.
#[non_exhaustive]
#[derive(Debug, Error)]
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

    /// Path limits for a working-tree comparison in a repository without a work tree.
    #[error("grit diff needs a working tree")]
    NoWorkTree,

    /// A revision operand does not name an object.
    #[error("could not resolve '{spec}'")]
    UnresolvedRevision {
        /// The operand as given.
        spec: String,
        /// Why resolution failed.
        #[source]
        source: Box<Error>,
    },

    /// A revision (after peeling tags) names a tree or blob instead of a commit.
    #[error("object {oid} does not name a commit")]
    NotACommit {
        /// The tree or blob the revision led to.
        oid: ObjectId,
    },

    /// A three-dot range whose sides share no history.
    #[error("no merge base for that revision range")]
    NoMergeBase,

    /// A three-dot range whose sides have more than one merge base (criss-cross history).
    #[error("multiple merge bases for that revision range")]
    MultipleMergeBases,

    /// A pathspec could not be parsed or normalized.
    #[error(transparent)]
    Pathspec(#[from] PathspecResolveError),

    /// Range endpoint resolution, object reads, or a working-tree pathspec matching nothing
    /// ([`Error::PathspecNoMatch`]).
    #[error(transparent)]
    Library(Box<Error>),
}

impl From<Error> for ResolveDiffArgumentsError {
    fn from(err: Error) -> Self {
        Self::Library(Box::new(err))
    }
}

type ResolveResult<T> = std::result::Result<T, ResolveDiffArgumentsError>;

/// Parse `grit diff` flags and positionals into a [`DiffTarget`] and pathspecs.
///
/// Shorthand for [`resolve_diff_request`] without a pathspec file.
///
/// # Parameters
///
/// - `repo` - repository the revisions and paths belong to.
/// - `cwd_prefix` - path from the work tree root to the process cwd (from
///   [`pathdiff`](crate::worktree::pathspec::pathdiff)); `None` at the root, when unknown, or in
///   a bare repository.
/// - `staged`, `unstaged`, `against` - see [`DiffRequest`].
/// - `positionals` - operands before `--`.
/// - `explicit_paths` - paths after `--`.
///
/// # Errors
///
/// Returns [`ResolveDiffArgumentsError`] for flag conflicts, unresolvable revisions, or
/// pathspecs that cannot be resolved.
pub fn resolve_diff_arguments(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    staged: bool,
    unstaged: bool,
    against: Option<&str>,
    positionals: &[String],
    explicit_paths: &[String],
) -> ResolveResult<DiffArguments> {
    let request = DiffRequest {
        staged,
        unstaged,
        against: against.map(str::to_owned),
        positionals: positionals.to_vec(),
        paths: explicit_paths.to_vec(),
        pathspec_file: None,
    };
    resolve_diff_request(repo, cwd_prefix, &request)
}

/// Decide what `grit diff` compares and which paths it is limited to.
///
/// Operands before `--` that are not revisions but name known paths (in the index, `HEAD`, or
/// the working tree) are treated as paths, so `grit diff file.txt` and `grit diff HEAD~1
/// file.txt` work without `--`. One operand selects a commit's own patch, or a tree diff for
/// `A..B` / `A...B` (merge base of `A` and `B` against `B`); two operands compare two commits.
/// With no operands the working tree is compared with `HEAD`, or with
/// [`DiffRequest::against`].
///
/// Working-tree pathspecs must match a known path; commit and range pathspecs only need to
/// parse, so paths deleted on either side can still be named.
///
/// # Parameters
///
/// - `repo` - repository the revisions and paths belong to.
/// - `cwd_prefix` - path from the work tree root to the process cwd (from
///   [`pathdiff`](crate::worktree::pathspec::pathdiff)); `None` at the root, when unknown, or in
///   a bare repository.
/// - `request` - flags and operands as given.
///
/// # Errors
///
/// - Flag conflicts: [`ResolveDiffArgumentsError::StagedAndUnstaged`],
///   [`StagedWithRevisions`](ResolveDiffArgumentsError::StagedWithRevisions),
///   [`UnstagedWithRevisions`](ResolveDiffArgumentsError::UnstagedWithRevisions),
///   [`TooManyRevisions`](ResolveDiffArgumentsError::TooManyRevisions),
///   [`AgainstWithCommits`](ResolveDiffArgumentsError::AgainstWithCommits),
///   [`RangeWithSecondRevision`](ResolveDiffArgumentsError::RangeWithSecondRevision).
/// - Revisions: [`UnresolvedRevision`](ResolveDiffArgumentsError::UnresolvedRevision),
///   [`NotACommit`](ResolveDiffArgumentsError::NotACommit),
///   [`NoMergeBase`](ResolveDiffArgumentsError::NoMergeBase),
///   [`MultipleMergeBases`](ResolveDiffArgumentsError::MultipleMergeBases).
/// - Paths: [`NoWorkTree`](ResolveDiffArgumentsError::NoWorkTree),
///   [`Pathspec`](ResolveDiffArgumentsError::Pathspec), or
///   [`Library`](ResolveDiffArgumentsError::Library) wrapping [`Error::PathspecNoMatch`].
pub fn resolve_diff_request(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    request: &DiffRequest,
) -> ResolveResult<DiffArguments> {
    let (commits, path_args) = disambiguate_diff_revision_and_paths(
        repo,
        cwd_prefix,
        request.positionals.clone(),
        request.paths.clone(),
    );
    let file_paths = request.pathspec_file.as_deref().unwrap_or_default();
    let path_tokens = [file_paths, path_args.as_slice()].concat();
    if (request.pathspec_file.is_some() || !request.paths.is_empty())
        && (path_tokens.is_empty() || path_tokens.iter().any(String::is_empty))
    {
        return Err(PathspecResolveError::EmptyPathspec.into());
    }
    let against = request.against.as_deref();

    if request.staged && request.unstaged {
        return Err(ResolveDiffArgumentsError::StagedAndUnstaged);
    }
    if request.staged || request.unstaged {
        if !commits.is_empty() || against.is_some() {
            return Err(if request.staged {
                ResolveDiffArgumentsError::StagedWithRevisions
            } else {
                ResolveDiffArgumentsError::UnstagedWithRevisions
            });
        }
        return Ok(DiffArguments {
            target: if request.staged {
                DiffTarget::IndexVsHead
            } else {
                DiffTarget::WorktreeVsIndex
            },
            pathspecs: resolve_worktree_diff_pathspecs(repo, cwd_prefix, &path_tokens)?,
        });
    }
    if commits.len() > 2 {
        return Err(ResolveDiffArgumentsError::TooManyRevisions);
    }
    if against.is_some() && !commits.is_empty() {
        return Err(ResolveDiffArgumentsError::AgainstWithCommits);
    }

    let trees = |old: ObjectId, new: ObjectId, pathspecs: Vec<String>| DiffArguments {
        target: DiffTarget::Trees {
            old: Some(old),
            new,
        },
        pathspecs,
    };
    match commits.as_slice() {
        [] => {
            let pathspecs = resolve_worktree_diff_pathspecs(repo, cwd_prefix, &path_tokens)?;
            let target = match against {
                Some(spec) => DiffTarget::WorktreeVsTree(commit_tree(
                    repo,
                    &resolve_diff_commit(repo, spec)?,
                )?),
                None => DiffTarget::WorktreeVsHead,
            };
            Ok(DiffArguments { target, pathspecs })
        }
        [spec] => {
            if let Some((base, tip)) = parse_commit_range(repo, spec)? {
                let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_tokens)?;
                return Ok(trees(base, tip, pathspecs));
            }
            match resolve_diff_commit(repo, spec) {
                Ok(oid) => Ok(DiffArguments {
                    target: DiffTarget::Commit(oid),
                    pathspecs: resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_tokens)?,
                }),
                // Neither a revision nor (per disambiguation) a known path: report it as a path.
                Err(_) if path_args.is_empty() => Ok(DiffArguments {
                    target: DiffTarget::WorktreeVsIndex,
                    pathspecs: resolve_worktree_diff_pathspecs(
                        repo,
                        cwd_prefix,
                        std::slice::from_ref(spec),
                    )?,
                }),
                Err(err) => Err(err),
            }
        }
        [first, second] => {
            if let Some((base, tip)) = parse_commit_range(repo, first)? {
                if !token_is_known_worktree_pathspec(repo, cwd_prefix, second) {
                    resolve_diff_commit(repo, second)?;
                    return Err(ResolveDiffArgumentsError::RangeWithSecondRevision);
                }
                let range_paths = [file_paths, std::slice::from_ref(second)].concat();
                let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &range_paths)?;
                return Ok(trees(base, tip, pathspecs));
            }
            let base = resolve_diff_commit(repo, first)?;
            let tip = resolve_diff_commit(repo, second)?;
            let pathspecs = resolve_commit_diff_pathspecs(repo, cwd_prefix, &path_tokens)?;
            Ok(trees(base, tip, pathspecs))
        }
        _ => Err(ResolveDiffArgumentsError::TooManyRevisions),
    }
}

fn include_file_in_diff(file: &FileDiff, opts: &DiffOptions) -> bool {
    if !opts.whitespace.is_active() || file.binary {
        return true;
    }
    if matches!(
        file.status,
        DiffStatus::Renamed
            | DiffStatus::Added
            | DiffStatus::Deleted
            | DiffStatus::Copied
            | DiffStatus::TypeChanged
    ) {
        return true;
    }
    if file.old_mode != file.new_mode {
        return true;
    }
    !file.hunks.is_empty()
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
            )?;
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts.clone())?
        }
        DiffTarget::WorktreeVsTree(tree) => {
            let changes = filter_file_changes(
                worktree_changes(repo, Some(tree), rename_threshold)?,
                repo,
                pathspecs,
            )?;
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts.clone())?
        }
        DiffTarget::IndexVsHead => {
            let restrict = pathspec_restrict_set(pathspecs);
            let changes = filter_file_changes(
                staged_changes(repo, rename_threshold, restrict)?,
                repo,
                pathspecs,
            )?;
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts.clone())?
        }
        DiffTarget::WorktreeVsIndex => {
            let changes =
                filter_file_changes(unstaged_changes(repo, rename_threshold)?, repo, pathspecs)?;
            file_diffs_from_worktree_changes(changes, &config, &thread_policy, opts.clone())?
        }
        DiffTarget::Commit(oid) => {
            let items = commit_changes(
                repo,
                oid,
                &config,
                &thread_policy,
                opts.clone(),
                rename_threshold,
            )?;
            filter_and_map_tree_structured(items, repo, pathspecs, opts)?
        }
        DiffTarget::Trees { old, new } => {
            let items = if let Some(base) = old {
                tree_commit_changes(
                    repo,
                    base,
                    new,
                    &config,
                    &thread_policy,
                    opts.clone(),
                    rename_threshold,
                )?
            } else {
                commit_changes(
                    repo,
                    new,
                    &config,
                    &thread_policy,
                    opts.clone(),
                    rename_threshold,
                )?
            };
            filter_and_map_tree_structured(items, repo, pathspecs, opts)?
        }
    };
    Ok(files)
}

fn filter_and_map_tree_structured(
    items: Vec<TreeDiffStructured>,
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
) -> Result<Vec<FileDiff>> {
    Ok(filter_tree_diff(items, repo, pathspecs)?
        .into_iter()
        .map(file_diff_from_tree_structured)
        .filter(|f| include_file_in_diff(f, &opts))
        .collect())
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
pub(crate) struct FileChange {
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
    pub(crate) status: DiffStatus,
    pub(crate) old_mode: String,
    pub(crate) new_mode: String,
    pub(crate) old_oid: ObjectId,
    pub(crate) new_oid: ObjectId,
    pub(crate) old_bytes: SideBytes,
    pub(crate) new_bytes: SideBytes,
    pub(crate) binary: bool,
    pub(crate) content_changed: bool,
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
        build_file_diff(fc, opts.clone())
    })
    .into_iter()
    .filter(|f| include_file_in_diff(f, &opts))
    .collect())
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
    let Ok(resolved) = repo.resolve_worktree_pathspec(token, cwd_prefix) else {
        return false;
    };
    pathspec_matches_known_in_repository(repo, &resolved).unwrap_or(false)
}

/// Move operands that clap placed before `--` but that name paths rather than revisions.
fn disambiguate_diff_revision_and_paths(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    mut commits: Vec<String>,
    mut paths: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    if !paths.is_empty() {
        return (commits, paths);
    }
    let is_path = |token: &str| token_is_known_worktree_pathspec(repo, cwd_prefix, token);
    match commits.as_slice() {
        [first, second]
            if token_is_diff_revision(repo, first)
                && !token_is_diff_revision(repo, second)
                && is_path(second) =>
        {
            paths.extend(commits.pop());
        }
        [only] if !token_is_diff_revision(repo, only) && is_path(only) => {
            paths.extend(commits.pop());
        }
        _ => {}
    }
    if paths.is_empty() {
        while let [_, .., tail] = commits.as_slice() {
            if token_is_diff_revision(repo, tail) || !is_path(tail) {
                break;
            }
            paths.extend(commits.pop());
        }
    }
    (commits, paths)
}

fn pathspec_resolution_root(repo: &Repository) -> &Path {
    repo.work_tree().unwrap_or_else(|| repo.git_dir())
}

/// Working-tree diffs: every pathspec must match a path in the index, `HEAD`, or on disk.
fn resolve_worktree_diff_pathspecs(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    raw_paths: &[String],
) -> ResolveResult<Vec<String>> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    if repo.work_tree().is_none() {
        return Err(ResolveDiffArgumentsError::NoWorkTree);
    }
    raw_paths
        .iter()
        .map(|pathspec| {
            let resolved = repo.resolve_worktree_pathspec(pathspec, cwd_prefix)?;
            ensure_pathspec_matches_known_in_repository(repo, pathspec, &resolved)?;
            Ok(resolved)
        })
        .collect()
}

/// Normalize pathspecs for a commit or range diff to repository-relative form.
///
/// Unlike working-tree diffs, the paths need not exist in the current checkout (a path deleted
/// on the tip, or exclude magic, is fine), and a bare repository resolves them against the git
/// directory.
///
/// # Parameters
///
/// - `repo` - repository the paths belong to.
/// - `cwd_prefix` - path from the work tree root to the process cwd; `None` at the root, when
///   unknown, or in a bare repository.
/// - `raw_paths` - pathspecs as given.
///
/// # Errors
///
/// Returns [`ResolveDiffArgumentsError::Pathspec`] when a pathspec cannot be parsed or lies
/// outside the work tree.
pub fn resolve_commit_diff_pathspecs(
    repo: &Repository,
    cwd_prefix: Option<&str>,
    raw_paths: &[String],
) -> ResolveResult<Vec<String>> {
    if repo.work_tree().is_none() {
        let root = pathspec_resolution_root(repo);
        return Ok(raw_paths
            .iter()
            .map(|pathspec| resolve_pathspec(pathspec, root, cwd_prefix))
            .collect());
    }
    raw_paths
        .iter()
        .map(|pathspec| Ok(repo.resolve_worktree_pathspec(pathspec, cwd_prefix)?))
        .collect()
}

fn filter_file_changes(
    changes: Vec<FileChange>,
    repo: &Repository,
    specs: &[String],
) -> Result<Vec<FileChange>> {
    if specs.is_empty() {
        return Ok(changes);
    }
    let matcher = repo.pathspec_matcher(specs)?;
    Ok(changes
        .into_iter()
        .filter(|c| matcher.matches_change(&c.path, c.old_path.as_deref()))
        .collect())
}

fn diff_entry_matches(matcher: &PathspecMatcher<'_>, entry: &DiffEntry) -> bool {
    let path = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("");
    matcher.matches_change(path, entry.old_path.as_deref())
}

fn filter_tree_diff(
    items: Vec<TreeDiffStructured>,
    repo: &Repository,
    specs: &[String],
) -> Result<Vec<TreeDiffStructured>> {
    if specs.is_empty() {
        return Ok(items);
    }
    let matcher = repo.pathspec_matcher(specs)?;
    Ok(items
        .into_iter()
        .filter(|item| diff_entry_matches(&matcher, &item.entry))
        .collect())
}

fn pathspec_restrict_set(pathspecs: &[String]) -> Option<Arc<BTreeSet<String>>> {
    if pathspecs.is_empty() {
        None
    } else {
        Some(Arc::new(pathspecs.iter().cloned().collect()))
    }
}

fn unstaged_changes(repo: &Repository, rename_threshold: u32) -> Result<Vec<FileChange>> {
    unstaged_changes_inner(repo, rename_threshold, None)
}

fn unstaged_changes_inner(
    repo: &Repository,
    rename_threshold: u32,
    restrict_to_paths: Option<Arc<BTreeSet<String>>>,
) -> Result<Vec<FileChange>> {
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
    let narrow = restrict_to_paths.is_some();
    let diff_opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        index_mtime: diff_index_mtime,
        config: Some(config_arc.clone()),
        thread_policy: Some(repo.thread_policy()),
        refresh_index_stat_in_pass: false,
        for_status: !narrow,
        refresh_fsmonitor: if narrow {
            false
        } else {
            fsmonitor_enabled(config_arc.as_ref())
        },
        worktree_rules: Some(Arc::clone(&rules_arc)),
        command_runner: Some(repo.command_runner()),
        restrict_to_paths,
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

fn tree_blob_at_path(odb: &Odb, tree: &ObjectId, path: &str) -> Result<Option<(ObjectId, u32)>> {
    let mut current = *tree;
    let mut components = path.split('/').peekable();
    while let Some(name) = components.next() {
        let raw = odb.read(&current)?;
        let entries = parse_tree(&raw.data)?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.name.as_slice() == name.as_bytes())
        else {
            return Ok(None);
        };
        if components.peek().is_some() {
            if entry.mode != MODE_TREE {
                return Ok(None);
            }
            current = entry.oid;
        } else {
            return Ok(Some((entry.oid, entry.mode)));
        }
    }
    Ok(None)
}

fn diff_index_to_tree_for_paths(
    repo: &Repository,
    head_tree: Option<&ObjectId>,
    paths: &std::collections::BTreeSet<String>,
) -> Result<Vec<DiffEntry>> {
    let odb = repo.odb();
    let index = repo.index_handle().load()?;
    let mut result = Vec::new();
    for path in paths {
        let index_entry = index.get(path.as_bytes(), 0);
        if index_entry.is_some_and(|e| e.intent_to_add()) {
            continue;
        }
        let tree_side = match head_tree {
            Some(t) => tree_blob_at_path(odb, t, path)?,
            None => None,
        };
        let (idx_oid, idx_mode) = index_entry
            .map(|e| (e.oid, e.mode))
            .unwrap_or((ObjectId::zero(), 0));
        let (tree_oid, tree_mode) = tree_side.unwrap_or((ObjectId::zero(), 0));
        if index_entry.is_none() && tree_side.is_none() {
            continue;
        }
        if index_entry.is_some() && tree_side.is_none() {
            result.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path.clone()),
                old_mode: "000000".to_owned(),
                new_mode: crate::diff::format_mode(idx_mode),
                old_oid: ObjectId::zero(),
                new_oid: idx_oid,
                score: None,
            });
            continue;
        }
        if index_entry.is_none() {
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path.clone()),
                new_path: None,
                old_mode: crate::diff::format_mode(tree_mode),
                new_mode: "000000".to_owned(),
                old_oid: tree_oid,
                new_oid: ObjectId::zero(),
                score: None,
            });
            continue;
        }
        if idx_oid != tree_oid || idx_mode != tree_mode {
            result.push(DiffEntry {
                status: DiffStatus::for_content_change(tree_mode, idx_mode),
                old_path: Some(path.clone()),
                new_path: Some(path.clone()),
                old_mode: crate::diff::format_mode(tree_mode),
                new_mode: crate::diff::format_mode(idx_mode),
                old_oid: tree_oid,
                new_oid: idx_oid,
                score: None,
            });
        }
    }
    Ok(result)
}

fn staged_changes(
    repo: &Repository,
    rename_threshold: u32,
    restrict_to_paths: Option<Arc<std::collections::BTreeSet<String>>>,
) -> Result<Vec<FileChange>> {
    let head = resolve_head(repo.git_dir())?;
    let head_tree = match head.oid() {
        Some(oid) => Some(commit_tree(repo, oid)?),
        None => None,
    };
    let index = repo.index_handle().load()?;
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    let mut entries = if let Some(paths) = restrict_to_paths.as_ref().filter(|p| !p.is_empty()) {
        diff_index_to_tree_for_paths(repo, head_tree.as_ref(), paths)?
    } else {
        diff_index_to_tree_with_renames(
            repo.odb(),
            &index,
            head_tree.as_ref(),
            false,
            rename_threshold,
            config.as_ref(),
            &thread_policy,
        )?
    };
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

    let cache_valid =
        crate::index::write_tree::cache_tree_extension_valid(index.cache_tree.as_ref());
    let index_backed = base_tree
        .zip(index.cache_tree_root.as_ref())
        .map(|(left, right)| left == right)
        .unwrap_or(base_tree.is_none() && index.cache_tree_root.is_none())
        && cache_valid;
    let diff_index_mtime = index_mtime_for_diff(&index, None);
    let rules_arc = Arc::new(Mutex::new(WorktreeRules::from_repository(repo, &index)?));
    let index_to_worktree_opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: Some(config.clone()),
        index_mtime: diff_index_mtime,
        thread_policy: Some(repo.thread_policy()),
        refresh_index_stat_in_pass: false,
        for_status: false,
        refresh_fsmonitor: fsmonitor_enabled(config.as_ref()),
        worktree_rules: Some(Arc::clone(&rules_arc)),
        command_runner: Some(repo.command_runner()),
        ..DiffIndexToWorktreeOptions::default()
    };
    let mut entries = if index_backed {
        let (entries, _) = diff_index_to_worktree_with_renames(
            repo,
            &mut index,
            work_tree,
            index_to_worktree_opts,
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

fn tree_commit_diff_entries(
    repo: &Repository,
    base: &ObjectId,
    tip: &ObjectId,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        base,
        tip,
        rename_threshold,
        config,
        thread_policy,
    )?;
    sort_entries(&mut entries);
    Ok(entries)
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
    let entries =
        tree_commit_diff_entries(repo, base, tip, config, thread_policy, rename_threshold)?;
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
}

fn commit_diff_entries(
    repo: &Repository,
    oid: &ObjectId,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
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
    Ok(entries)
}

fn commit_changes(
    repo: &Repository,
    oid: &ObjectId,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    diff_options: DiffOptions,
    rename_threshold: u32,
) -> Result<Vec<TreeDiffStructured>> {
    let entries = commit_diff_entries(repo, oid, config, thread_policy, rename_threshold)?;
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
}

fn peel_diff_commit(repo: &Repository, oid: ObjectId) -> ResolveResult<ObjectId> {
    peel_tags_to_commit(repo, oid)?.map_err(|oid| ResolveDiffArgumentsError::NotACommit { oid })
}

/// Resolve a revision operand to a commit, peeling annotated tags.
fn resolve_diff_commit(repo: &Repository, spec: &str) -> ResolveResult<ObjectId> {
    let oid = resolve_revision(repo, spec).map_err(|source| {
        ResolveDiffArgumentsError::UnresolvedRevision {
            spec: spec.to_owned(),
            source: Box::new(source),
        }
    })?;
    peel_diff_commit(repo, oid)
}

/// Resolve one side of `A..B` / `A...B`; an empty side means `HEAD`.
fn resolve_diff_range_end(repo: &Repository, spec: &str) -> ResolveResult<ObjectId> {
    let spec = if spec.is_empty() { "HEAD" } else { spec };
    peel_diff_commit(repo, resolve_revision_for_range_end(repo, spec)?)
}

/// `(old, new)` commits for a two-dot or three-dot range, or `None` when `spec` is not a range.
fn parse_commit_range(
    repo: &Repository,
    spec: &str,
) -> ResolveResult<Option<(ObjectId, ObjectId)>> {
    if let Some((left, right)) = split_triple_dot_range(spec) {
        let left = resolve_diff_range_end(repo, left)?;
        let right = resolve_diff_range_end(repo, right)?;
        let base = merge_base_for_diff_two_commits(repo, left, right).map_err(|e| match e {
            MergeBaseForDiffError::None => ResolveDiffArgumentsError::NoMergeBase,
            MergeBaseForDiffError::Multiple => ResolveDiffArgumentsError::MultipleMergeBases,
            MergeBaseForDiffError::Other(msg) => Error::Message(msg).into(),
        })?;
        return Ok(Some((base, right)));
    }
    let Some((left, right)) = split_double_dot_range(spec) else {
        return Ok(None);
    };
    Ok(Some((
        resolve_diff_range_end(repo, left)?,
        resolve_diff_range_end(repo, right)?,
    )))
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

/// Unstaged file diffs with staged peer hunks per path (for stable hunk ids).
pub(crate) type UnstagedDiffWithPeers = (
    Vec<FileDiff>,
    std::collections::HashMap<String, Vec<StructuredHunk>>,
);

pub(crate) type UnstagedRawWithPeers = (
    Vec<FileChange>,
    std::collections::HashMap<String, Vec<StructuredHunk>>,
);

/// Raw unstaged [`FileChange`] rows plus staged peer hunks (no structured diff built yet).
pub(crate) fn unstaged_raw_changes_with_peer_hunks(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
) -> Result<UnstagedRawWithPeers> {
    let rename_threshold = opts.renames.threshold_percent().unwrap_or(101);
    let peer_hunks =
        staged_peer_hunks_for_pathspecs(repo, pathspecs, opts.clone(), rename_threshold)?;
    let restrict = exact_file_literal_pathspec_arc(repo, pathspecs)?;
    let unstaged = if let Some(paths) = restrict {
        unstaged_changes_inner(repo, rename_threshold, Some(paths))?
    } else {
        filter_file_changes(unstaged_changes(repo, rename_threshold)?, repo, pathspecs)?
    };
    Ok((unstaged, peer_hunks))
}

fn blob_oid_in_tree(repo: &Repository, tree: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    use crate::objects::parse_tree;
    let mut cur = tree;
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for (i, name) in components.iter().enumerate() {
        let raw = repo.odb().read(&cur)?;
        let entries = parse_tree(&raw.data)?;
        let name_bytes = name.as_bytes();
        let Some(entry) = entries.iter().find(|e| e.name.as_slice() == name_bytes) else {
            return Ok(None);
        };
        if i + 1 == components.len() {
            return Ok(Some(entry.oid));
        }
        if entry.mode != crate::index::MODE_TREE {
            return Ok(None);
        }
        cur = entry.oid;
    }
    Ok(None)
}

fn path_has_staged_delta_vs_head_with_index(
    repo: &Repository,
    index: &Index,
    head_tree: Option<ObjectId>,
    path: &str,
) -> Result<bool> {
    let path_bytes = path.as_bytes();
    if index.get(path_bytes, 2).is_some() || index.get(path_bytes, 3).is_some() {
        return Ok(true);
    }
    let head_blob = match head_tree {
        Some(tree) => blob_oid_in_tree(repo, tree, path)?,
        None => None,
    };
    let Some(idx_entry) = index.get(path_bytes, 0) else {
        return Ok(head_blob.is_some());
    };
    match head_blob {
        None => Ok(true),
        Some(h) => Ok(idx_entry.oid != h),
    }
}

fn exact_file_literal_pathspec_arc(
    repo: &Repository,
    pathspecs: &[String],
) -> Result<Option<Arc<std::collections::BTreeSet<String>>>> {
    let Some(literal_paths) =
        exact_literal_pathspec_set(pathspecs, PathspecMatchContext::default())
    else {
        return Ok(None);
    };
    let index = repo.index_handle().load()?;
    let head = resolve_head(repo.git_dir())?;
    let head_tree = match head.oid() {
        Some(oid) => Some(commit_tree(repo, oid)?),
        None => None,
    };
    if literal_paths
        .iter()
        .all(|p| path_is_exact_tracked_file(repo, &index, head_tree, p).unwrap_or(false))
    {
        Ok(Some(Arc::new(literal_paths)))
    } else {
        Ok(None)
    }
}

fn path_is_exact_tracked_file(
    repo: &Repository,
    index: &Index,
    head_tree: Option<ObjectId>,
    path: &str,
) -> Result<bool> {
    let path_bytes = path.as_bytes();
    if let Some(entry) = index.get(path_bytes, 0) {
        return Ok(entry.mode != MODE_TREE);
    }
    match head_tree {
        Some(tree) => Ok(blob_oid_in_tree(repo, tree, path)?.is_some()),
        None => Ok(false),
    }
}

fn staged_peer_hunks_for_pathspecs(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
    rename_threshold: u32,
) -> Result<std::collections::HashMap<String, Vec<StructuredHunk>>> {
    if let Some(literal_paths) =
        exact_literal_pathspec_set(pathspecs, PathspecMatchContext::default())
    {
        let index = repo.index_handle().load()?;
        let head = resolve_head(repo.git_dir())?;
        let head_tree = match head.oid() {
            Some(oid) => Some(commit_tree(repo, oid)?),
            None => None,
        };
        let all_exact_files = literal_paths
            .iter()
            .all(|p| path_is_exact_tracked_file(repo, &index, head_tree, p).unwrap_or(false));
        if all_exact_files
            && literal_paths.iter().all(|p| {
                path_has_staged_delta_vs_head_with_index(repo, &index, head_tree, p)
                    .map(|delta| !delta)
                    .unwrap_or(false)
            })
        {
            return Ok(std::collections::HashMap::new());
        }
    }
    let staged = filter_file_changes(
        staged_changes(repo, rename_threshold, None)?,
        repo,
        pathspecs,
    )?;
    Ok(staged
        .iter()
        .map(|fc| {
            let structured = structured_for_file_change(fc, opts.clone());
            (fc.path.clone(), structured.hunks)
        })
        .collect())
}

/// Unstaged [`FileDiff`] rows plus staged hunks on the same path (for stable hunk ids).
pub(crate) fn unstaged_file_diffs_with_peer_hunks(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
) -> Result<UnstagedDiffWithPeers> {
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    let rename_threshold = opts.renames.threshold_percent().unwrap_or(101);
    let staged = filter_file_changes(
        staged_changes(repo, rename_threshold, None)?,
        repo,
        pathspecs,
    )?;
    let peer_hunks: std::collections::HashMap<String, Vec<StructuredHunk>> = staged
        .iter()
        .map(|fc| {
            let structured = structured_for_file_change(fc, opts.clone());
            (fc.path.clone(), structured.hunks)
        })
        .collect();
    let unstaged = filter_file_changes(unstaged_changes(repo, rename_threshold)?, repo, pathspecs)?;
    let files = file_diffs_from_worktree_changes(unstaged, &config, &thread_policy, opts)?;
    Ok((files, peer_hunks))
}

/// Staged and unstaged file diffs for hunk id assignment (one staged scan per call).
pub struct HunkDiffBundle {
    /// Unstaged side file rows.
    pub unstaged: Vec<FileDiff>,
    /// Staged hunks on the same path (for peer-aware ids).
    pub peer_hunks: std::collections::HashMap<String, Vec<StructuredHunk>>,
}

/// Load unstaged diffs, staged peer hunks, and staged-side file rows in one pass.
pub fn hunk_diff_bundle(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
) -> Result<HunkDiffBundle> {
    let config = repo.config()?;
    let thread_policy = repo.thread_policy();
    let rename_threshold = opts.renames.threshold_percent().unwrap_or(101);
    // Pathspec semantics belong in `filter_file_changes`; do not pass raw pathspec strings
    // to `restrict_to_paths` (glob/magic patterns are not repository paths).
    let staged_fc = filter_file_changes(
        staged_changes(repo, rename_threshold, None)?,
        repo,
        pathspecs,
    )?;
    let staged_files = if staged_fc.is_empty() {
        Vec::new()
    } else {
        file_diffs_from_worktree_changes(staged_fc, &config, &thread_policy, opts.clone())?
    };
    let peer_hunks: std::collections::HashMap<String, Vec<StructuredHunk>> = staged_files
        .iter()
        .map(|f| (f.path.clone(), f.hunks.clone()))
        .collect();
    let unstaged_fc =
        filter_file_changes(unstaged_changes(repo, rename_threshold)?, repo, pathspecs)?;
    let unstaged = file_diffs_from_worktree_changes(unstaged_fc, &config, &thread_policy, opts)?;
    Ok(HunkDiffBundle {
        unstaged,
        peer_hunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_file_omits_whitespace_only_modified() {
        let file = FileDiff {
            path: "x.txt".into(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: Some("100644".into()),
            new_mode: Some("100644".into()),
            old_oid: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
            new_oid: ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap(),
            binary: false,
            content_changed: true,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: Vec::new(),
        };
        let opts =
            DiffOptions::default().with_whitespace(crate::diff::WhitespaceMode::ignore_all_space());
        assert!(!include_file_in_diff(&file, &opts));
    }

    #[test]
    fn include_file_omits_ws_only_even_when_content_changed_false() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
        let file = FileDiff {
            path: "x.txt".into(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: Some("100644".into()),
            new_mode: Some("100644".into()),
            old_oid: oid,
            new_oid: oid,
            binary: false,
            content_changed: false,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: Vec::new(),
        };
        let opts =
            DiffOptions::default().with_whitespace(crate::diff::WhitespaceMode::ignore_all_space());
        assert!(!include_file_in_diff(&file, &opts));
    }

    #[test]
    fn include_file_keeps_mode_only_under_whitespace() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
        let file = FileDiff {
            path: "x.txt".into(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: Some("100644".into()),
            new_mode: Some("100755".into()),
            old_oid: oid,
            new_oid: oid,
            binary: false,
            content_changed: false,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: Vec::new(),
        };
        let opts =
            DiffOptions::default().with_whitespace(crate::diff::WhitespaceMode::ignore_all_space());
        assert!(include_file_in_diff(&file, &opts));
    }

    #[test]
    fn include_file_keeps_whitespace_only_rename() {
        let file = FileDiff {
            path: "new.txt".into(),
            old_path: Some("old.txt".into()),
            status: DiffStatus::Renamed,
            old_mode: Some("100644".into()),
            new_mode: Some("100644".into()),
            old_oid: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
            new_oid: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
            binary: false,
            content_changed: false,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: Vec::new(),
        };
        let opts =
            DiffOptions::default().with_whitespace(crate::diff::WhitespaceMode::ignore_all_space());
        assert!(include_file_in_diff(&file, &opts));
    }

    #[test]
    fn resolve_errors_staged_and_unstaged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::repo::Repository::init(dir.path(), &crate::repo::InitOptions::default())
            .expect("init");
        let err = resolve_diff_arguments(&repo, None, true, true, None, &[], &[]).unwrap_err();
        assert!(matches!(err, ResolveDiffArgumentsError::StagedAndUnstaged));
    }
}

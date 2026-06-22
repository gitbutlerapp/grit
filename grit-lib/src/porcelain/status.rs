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

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::diff::DiffEntry;
use crate::error::Result;
use crate::ignore::IgnoreMatcher;
use crate::index::{Index, MODE_GITLINK, MODE_TREE};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::state::{HeadState, WtStatusState};

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
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            untracked: UntrackedMode::Normal,
            ignored: IgnoredMode::No,
            renames: None,
            pathspecs: Vec::new(),
            ahead_behind: true,
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

/// Walk the work tree and collect untracked and ignored paths.
///
/// `ignored_mode` selects whether (and how) ignored paths are reported;
/// `show_all` corresponds to `--untracked-files=all`. Results are sorted.
pub fn collect_untracked_and_ignored(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    ignored_mode: IgnoredMode,
    show_all: bool,
    pathspecs: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    // Keep parity with historical status behavior in tests that rely on broad untracked scans
    // (including detached-HEAD wtstatus cases): when no explicit pathspec is requested, avoid
    // pathspec-based pruning entirely.
    let effective_pathspecs: &[String] = if pathspecs.is_empty() { &[] } else { pathspecs };
    let tracked: BTreeSet<String> = index
        .entries
        .iter()
        .map(|ie| String::from_utf8_lossy(&ie.path).to_string())
        .collect();

    let gitlinks: BTreeSet<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();

    let mut matcher = IgnoreMatcher::from_repository(repo)?;
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();

    visit_untracked_node(
        repo,
        index,
        work_tree,
        &tracked,
        &gitlinks,
        &mut matcher,
        ignored_mode,
        show_all,
        "",
        work_tree,
        effective_pathspecs,
        &mut untracked,
        &mut ignored,
    )?;

    untracked.sort();
    ignored.sort();
    Ok((untracked, ignored))
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
    rel: &str,
    abs: &Path,
    pathspecs: &[String],
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
) -> Result<()> {
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
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let child_rel = relative_path(rel, &name);
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

        if is_dir && gitlinks.contains(&child_rel) {
            continue;
        }

        if tracked.contains(&child_rel) {
            continue;
        }

        if is_dir {
            if !pathspec_may_match_directory(&child_rel, pathspecs) {
                continue;
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
                &child_rel,
                &path,
                pathspecs,
                untracked_out,
                ignored_out,
            )?;
        } else {
            if !status_path_matches_worktree(repo, index, work_tree, &child_rel, pathspecs) {
                continue;
            }
            let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
            if is_ign {
                if ignored_mode != IgnoredMode::No {
                    ignored_out.push(child_rel);
                }
            } else {
                untracked_out.push(child_rel);
            }
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
    rel: &str,
    abs: &Path,
    pathspecs: &[String],
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
            rel,
            abs,
            pathspecs,
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
            repo, index, work_tree, tracked, gitlinks, matcher, rel, abs, pathspecs,
        )? {
            ignored_out.push(dir_line);
            return Ok(());
        }
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
        rel,
        abs,
        pathspecs,
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
            let rel_child = path
                .strip_prefix(work_tree)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| name.clone());
            if !pathspec_may_match_directory(&rel_child, pathspecs)
                && !(entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                    && status_path_matches_worktree(repo, index, work_tree, &rel_child, pathspecs))
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
    if pathspecs.is_empty() {
        return true;
    }
    if !pathspecs_use_attr_magic(pathspecs) {
        return status_path_matches(path, pathspecs);
    }

    let normalized = path.trim_end_matches('/');
    let attrs =
        crate::crlf::load_gitattributes_for_checkout(work_tree, normalized, index, &repo.odb);
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

    // Load the index, remembering whether it was sparse on disk, then expand
    // sparse-directory placeholders so the diffs see real entries.
    let index_path = repo.index_path();
    let mut index = match Index::load(&index_path) {
        Ok(i) => i,
        Err(crate::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };
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
    let mut staged: Vec<DiffEntry> =
        crate::diff::diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?
            .into_iter()
            .filter(|e| status_path_matches(&e.path().to_string_lossy(), &opts.pathspecs))
            .collect();

    // Unstaged: worktree vs index, narrowed before rename detection.
    let mut unstaged: Vec<DiffEntry> = crate::diff::diff_index_to_worktree_with_options(
        &repo.odb,
        &index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            ignore_submodule_untracked: opts.untracked == UntrackedMode::No,
            ..Default::default()
        },
    )?
    .into_iter()
    .filter(|e| status_path_matches(&e.path().to_string_lossy(), &opts.pathspecs))
    .collect();

    if let Some(rd) = opts.renames {
        staged = apply_status_renames(&repo.odb, staged, rd, head_tree.as_ref())?;
        unstaged = apply_status_renames(&repo.odb, unstaged, rd, head_tree.as_ref())?;
    }

    let (untracked, ignored) = if opts.untracked == UntrackedMode::No {
        (Vec::new(), Vec::new())
    } else {
        collect_untracked_and_ignored(
            repo,
            &index,
            work_tree,
            opts.ignored,
            opts.untracked == UntrackedMode::All,
            &opts.pathspecs,
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

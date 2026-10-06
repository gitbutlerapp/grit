//! Stage working-tree changes into the index (`git add`).
//!
//! [`stage`] performs a single index-vs-worktree scan (plus an untracked walk when
//! [`StageMode::All`] is selected). It does **not** compute the index-vs-HEAD diff
//! that full [`super::status::status`] requires.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::diff::{mode_from_metadata, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus};
use crate::error::{Error, Result};
use crate::index::{entry_from_stat, index_file_mtime, Index, MODE_TREE};
use crate::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use crate::pathspec::{has_glob_chars, matches_pathspec_list, pathspec_is_exclude};
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use crate::progress::ProgressSink;
use crate::repo::Repository;
use crate::state::resolve_head;

/// What paths [`stage`] should update (`git add` vs `git add -u`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageMode {
    /// Stage tracked changes and untracked files (default `git add`).
    #[default]
    All,
    /// Stage tracked changes only (`git add -u` / `--update`).
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
    pub mode: StageMode,
}

/// Counts returned by [`stage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageOutcome {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
}

impl StageOutcome {
    /// Total number of index paths updated.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added
            .saturating_add(self.modified)
            .saturating_add(self.removed)
    }
}

/// Stage paths matching `opts` into the index.
///
/// Loads the index, compares it to the work tree once, hashes only changed or new
/// blobs, removes deleted paths, and invalidates cache-tree / untracked-cache entries
/// for touched paths via [`Index::add_or_replace`] / [`Index::remove`].
///
/// # Errors
///
/// Returns [`Error::Message`] when an explicit pathspec matches nothing, or I/O /
/// object-store failures while reading the work tree.
pub fn stage(
    repo: &Repository,
    opts: &StageOptions,
    progress: &mut dyn ProgressSink,
) -> Result<StageOutcome> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    progress.start("stage", None);

    let index_path = repo.index_path();
    let index_mtime = index_file_mtime(&index_path);
    let mut index = repo.load_index()?;

    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ..DiffIndexToWorktreeOptions::default()
    };
    let unstaged =
        crate::diff::diff_index_to_worktree_with_options(&repo.odb, &index, work_tree, diff_opts)?;

    let untracked = if opts.mode == StageMode::All {
        let (untracked, _) = collect_untracked_and_ignored(
            repo,
            &index,
            work_tree,
            IgnoredMode::No,
            true,
            &opts.pathspecs,
        )?;
        untracked
    } else {
        Vec::new()
    };

    if !opts.pathspecs.is_empty() {
        validate_pathspecs(repo, &index, work_tree, &unstaged, &untracked, opts)?;
    }

    let matches =
        |path: &str| opts.pathspecs.is_empty() || matches_pathspec_list(path, &opts.pathspecs);

    let mut outcome = StageOutcome::default();

    for entry in &unstaged {
        let path = entry.path();
        if !matches(path) {
            continue;
        }
        match entry.status {
            DiffStatus::Deleted => {
                if index.remove(path.as_bytes()) {
                    outcome.removed += 1;
                }
            }
            DiffStatus::Modified | DiffStatus::TypeChanged | DiffStatus::Added => {
                stage_worktree_path(repo, work_tree, path, &mut index)?;
                outcome.modified += 1;
            }
            DiffStatus::Unmerged if entry.new_mode != "000000" => {
                stage_worktree_path(repo, work_tree, path, &mut index)?;
                outcome.modified += 1;
            }
            _ => {}
        }
    }

    for path in &untracked {
        if !matches(path) {
            continue;
        }
        stage_worktree_path(repo, work_tree, path, &mut index)?;
        outcome.added += 1;
    }

    if outcome.total() > 0 {
        index.sort();
        repo.write_index(&mut index)?;
    }

    progress.finish();
    Ok(outcome)
}

fn validate_pathspecs(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    unstaged: &[DiffEntry],
    untracked: &[String],
    opts: &StageOptions,
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
    let known = known_paths(repo, index, unstaged, untracked)?;
    for (label, resolved) in positive {
        if !selector_matches_known(resolved, &known, work_tree)? {
            return Err(Error::Message(format!(
                "pathspec '{label}' did not match any files"
            )));
        }
    }
    Ok(())
}

fn pathspec_sources(opts: &StageOptions) -> Vec<String> {
    if opts.pathspec_sources.is_empty() {
        opts.pathspecs.clone()
    } else {
        opts.pathspec_sources.clone()
    }
}

fn known_paths(
    repo: &Repository,
    index: &Index,
    unstaged: &[DiffEntry],
    untracked: &[String],
) -> Result<Vec<String>> {
    let mut set = HashSet::<String>::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            set.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    for entry in unstaged {
        set.insert(entry.path().to_owned());
    }
    for path in untracked {
        set.insert(path.clone());
    }
    for path in head_tree_paths(repo)? {
        set.insert(path);
    }
    Ok(set.into_iter().collect())
}

fn head_tree_paths(repo: &Repository) -> Result<Vec<String>> {
    let head = resolve_head(&repo.git_dir)?;
    let Some(head_oid) = head.oid() else {
        return Ok(Vec::new());
    };
    let obj = repo.odb.read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut paths = HashSet::new();
    collect_tree_paths(repo, commit.tree, "", &mut paths)?;
    Ok(paths.into_iter().collect())
}

fn collect_tree_paths(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut HashSet<String>,
) -> Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_TREE {
            collect_tree_paths(repo, entry.oid, &path, out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn selector_matches_known(resolved: &str, known: &[String], work_tree: &Path) -> Result<bool> {
    let spec = [resolved.to_owned()];
    if known.iter().any(|p| matches_pathspec_list(p, &spec)) {
        return Ok(true);
    }
    if has_glob_chars(resolved) || resolved.starts_with(":(") {
        return Ok(false);
    }
    if !resolved.starts_with(':') {
        let abs = work_tree.join(resolved);
        if abs.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn stage_worktree_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index: &mut Index,
) -> Result<()> {
    let abs = work_tree.join(rel_path);
    let meta = fs::symlink_metadata(&abs).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("could not read {rel_path}: {e}"),
        ))
    })?;
    let mode = mode_from_metadata(&meta);

    let data = if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("could not read symlink {rel_path}: {e}"),
            ))
        })?;
        target.to_string_lossy().into_owned().into_bytes()
    } else {
        fs::read(&abs).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("could not read {rel_path}: {e}"),
            ))
        })?
    };

    let oid = repo
        .odb
        .write(ObjectKind::Blob, &data)
        .map_err(|e| Error::Message(format!("could not store {rel_path}: {e}")))?;
    let entry = entry_from_stat(&abs, rel_path.as_bytes(), oid, mode)
        .map_err(|e| Error::Message(format!("could not stage {rel_path}: {e}")))?;
    index.add_or_replace(entry);
    if index.fsmonitor_last_update.is_some() {
        if let Some(staged) = index.get_mut(rel_path.as_bytes(), 0) {
            staged.set_fsmonitor_valid(true);
        }
    }
    Ok(())
}

/// Look up a cache-tree node by repository-relative path (for tests and diagnostics).
#[must_use]
pub fn cache_tree_node_valid(index: &Index, path: &str) -> Option<bool> {
    let root = index.cache_tree.as_ref()?;
    let node = find_cache_tree_node(root, path)?;
    Some(node.is_valid())
}

fn find_cache_tree_node<'a>(
    root: &'a crate::index::CacheTreeNode,
    path: &str,
) -> Option<&'a crate::index::CacheTreeNode> {
    if path.is_empty() {
        return Some(root);
    }
    let mut current = root;
    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        current = current
            .children
            .iter()
            .find(|c| c.name == component.as_bytes())?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::index::MODE_REGULAR;
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
        Repository::open(&git, Some(root)).unwrap()
    }

    fn stage_file_in_index(repo: &Repository, index: &mut Index, rel: &str, contents: &[u8]) {
        let wt = repo.work_tree.as_ref().unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, contents).unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, contents).unwrap();
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }

    fn write_index(repo: &Repository, index: &mut Index) {
        index.sort();
        repo.write_index(index).unwrap();
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
        assert_eq!(outcome.added, 1);
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.removed, 1);

        let index = repo.load_index().unwrap();
        assert!(index.get(b"fresh.txt", 0).is_some());
        assert!(index.get(b"modify.txt", 0).is_some());
        assert!(index.get(b"remove.txt", 0).is_none());
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
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.total(), 1);

        let index = repo.load_index().unwrap();
        let a_oid = index.get(b"a.txt", 0).unwrap().oid;
        let b_oid = index.get(b"b.txt", 0).unwrap().oid;
        let a_blob = repo.odb.read(&a_oid).unwrap();
        let b_blob = repo.odb.read(&b_oid).unwrap();
        assert_eq!(a_blob.data, b"a2\n");
        assert_eq!(b_blob.data, b"b\n");
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
    fn stage_skips_clean_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "clean.txt", b"same\n");
        stage_file_in_index(&repo, &mut index, "dirty.txt", b"old\n");
        write_index(&repo, &mut index);
        fs::write(root.join("dirty.txt"), b"new\n").unwrap();

        repo.odb.enable_mem_overlay();
        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(outcome.modified, 1);
        assert_eq!(outcome.total(), 1);

        let overlay_len = repo
            .odb
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
        let cache = crate::write_tree::build_cache_tree_from_index(&repo.odb, &index).unwrap();
        index.set_cache_tree(cache);
        write_index(&repo, &mut index);

        fs::write(root.join("beta/two.txt"), b"22\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let index = repo.load_index().unwrap();
        assert_eq!(cache_tree_node_valid(&index, ""), Some(false));
        assert_eq!(cache_tree_node_valid(&index, "alpha"), Some(true));
        assert_eq!(cache_tree_node_valid(&index, "beta"), Some(false));
    }
}

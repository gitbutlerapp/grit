//! Stage worktree changes into the index (`grit add` / commit `-a` engine).
//!
//! Uses index/worktree diffs instead of a full [`status`](super::status::status) pass so
//! `add` does not compute staged-vs-HEAD or ahead/behind when only staging is needed.

use std::path::Path;

use crate::config::ConfigSet;
use crate::crlf;
use crate::diff::{
    classify_worktree_entry_for_add, mode_from_metadata, DiffIndexToWorktreeOptions, DiffStatus,
    WorktreeAddRefresh, WorktreeAddRefreshParams,
};
use crate::error::Result;
use crate::index::{index_file_mtime, Index, MODE_TREE};
use crate::objects::ObjectKind;
use crate::pathspec::matches_pathspec_list;
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use crate::precompose_config::effective_core_precomposeunicode;
use crate::repo::Repository;
use crate::unicode_normalization::resolve_worktree_path_for_staging;

/// Stage paths matching `pathspecs` (empty = entire worktree).
///
/// Returns how many index entries were added, updated, or removed.
///
/// # Errors
///
/// Propagates I/O, diff, and object-database failures.
pub fn stage_worktree_changes(repo: &Repository, pathspecs: &[String]) -> Result<usize> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| crate::error::Error::Message("not a work tree".into()))?;

    let index_path = repo.index_path();
    let index_mtime = index_file_mtime(&index_path);
    let mut index = repo.load_index()?;
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let conv = crlf::ConversionConfig::from_config(&config);
    let attr_rules = crlf::load_gitattributes(work_tree);
    let precompose_unicode = effective_core_precomposeunicode(Some(&repo.git_dir));

    let matches = |path: &str| pathspecs.is_empty() || matches_pathspec_list(path, pathspecs);

    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: false,
        repository_git_dir: Some(repo.git_dir.clone()),
        ..Default::default()
    };
    let (unstaged, _) = crate::diff::diff_index_to_worktree_with_options(
        &repo.odb, &mut index, work_tree, diff_opts,
    )?;

    let mut staged = 0usize;
    for entry in unstaged {
        let path = entry.path();
        if !matches(path) {
            continue;
        }
        if entry.status == DiffStatus::Deleted {
            if index.remove(path.as_bytes()) {
                staged += 1;
            }
            continue;
        }
        if stage_tracked_worktree_path(
            repo,
            work_tree,
            path,
            &mut index,
            index_mtime,
            &conv,
            &attr_rules,
            &config,
            precompose_unicode,
        )? {
            staged += 1;
        }
    }

    let (untracked, _) =
        collect_untracked_and_ignored(repo, &index, work_tree, IgnoredMode::No, true, pathspecs)?;
    for path in untracked {
        if !matches(&path) {
            continue;
        }
        stage_new_worktree_path(repo, work_tree, &path, &mut index, precompose_unicode)?;
        staged += 1;
    }

    if staged > 0 {
        index.sort();
        repo.write_index(&mut index)?;
    }
    Ok(staged)
}

#[allow(clippy::too_many_arguments)]
fn stage_tracked_worktree_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index: &mut Index,
    index_mtime: Option<(u32, u32)>,
    conv: &crlf::ConversionConfig,
    attr_rules: &crlf::GitAttributes,
    config: &ConfigSet,
    precompose_unicode: bool,
) -> Result<bool> {
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    let staged_mode = mode_from_metadata(&meta);
    let Some(ie) = index.get(rel_path.as_bytes(), 0).cloned() else {
        return stage_new_worktree_path(repo, work_tree, rel_path, index, precompose_unicode)
            .map(|_| true);
    };

    let file_attrs = crlf::get_file_attrs(attr_rules, rel_path, false, config);
    let refresh = classify_worktree_entry_for_add(&WorktreeAddRefreshParams {
        odb: &repo.odb,
        ie: &ie,
        meta: &meta,
        abs_path: &abs,
        rel_path,
        conv,
        file_attrs: &file_attrs,
        index_mtime,
        staged_mode,
    })?;

    match refresh {
        WorktreeAddRefresh::UpToDate => Ok(false),
        WorktreeAddRefresh::ModeOnly { mode } => {
            if let Some(entry) = index.get_mut(rel_path.as_bytes(), 0) {
                entry.mode = mode;
            }
            Ok(true)
        }
        WorktreeAddRefresh::StatOnly => {
            let updated =
                crate::index::entry_from_metadata(&meta, rel_path.as_bytes(), ie.oid, ie.mode);
            index.add_or_replace(updated);
            Ok(true)
        }
        WorktreeAddRefresh::NeedsRestage => {
            stage_new_worktree_path(repo, work_tree, rel_path, index, precompose_unicode)?;
            Ok(true)
        }
    }
}

fn stage_new_worktree_path(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index: &mut Index,
    precompose_unicode: bool,
) -> Result<()> {
    if rel_path.ends_with('/') {
        return Ok(());
    }
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let index_relpath = resolved.index_relpath;
    let meta = std::fs::symlink_metadata(&abs).map_err(crate::error::Error::Io)?;
    if meta.is_dir() {
        return Ok(());
    }
    let mode = mode_from_metadata(&meta);
    if mode == MODE_TREE {
        return Ok(());
    }

    let data = if meta.file_type().is_symlink() {
        std::fs::read_link(&abs)
            .map_err(crate::error::Error::Io)?
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    } else {
        std::fs::read(&abs).map_err(crate::error::Error::Io)?
    };

    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    let entry = crate::index::entry_from_stat(&abs, index_relpath.as_bytes(), oid, mode)?;
    index.add_or_replace(entry);
    if index.fsmonitor_last_update.is_some() {
        if let Some(staged) = index.get_mut(index_relpath.as_bytes(), 0) {
            staged.set_fsmonitor_valid(true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn have_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Write with grit staging, verify index matches `git add` on the same tree.
    #[test]
    fn stage_one_file_matches_git_index() {
        if !have_git() {
            return;
        }
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let git_dir = wt.join(".git");
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(wt)
            .output()
            .expect("git init");
        std::fs::write(wt.join("a.txt"), b"one\n").expect("write");
        std::fs::write(wt.join("b.txt"), b"two\n").expect("write");
        Command::new("git")
            .args(["add", "a.txt", "b.txt"])
            .current_dir(wt)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(wt)
            .output()
            .expect("commit");

        std::fs::write(wt.join("a.txt"), b"one changed\n").expect("modify");

        let repo = Repository::open(&git_dir, Some(wt)).expect("open grit repo");
        let n = stage_worktree_changes(&repo, &[]).expect("stage");
        assert_eq!(n, 1);

        let grit_index = repo.load_index().expect("grit index");
        let grit_blob = grit_index
            .get(b"a.txt", 0)
            .map(|e| e.oid)
            .expect("a.txt staged");

        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(wt)
            .output()
            .expect("git add");
        let git_index_path = git_dir.join("index");
        let git_index = Index::load(&git_index_path).expect("git index");
        let git_blob = git_index
            .get(b"a.txt", 0)
            .map(|e| e.oid)
            .expect("git a.txt");

        assert_eq!(grit_blob, git_blob);

        let fsck = Command::new("git")
            .args(["fsck"])
            .current_dir(wt)
            .output()
            .expect("fsck");
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stderr)
        );
    }
}

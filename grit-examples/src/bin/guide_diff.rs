//! Tree-to-tree, index-to-worktree, and blob diffs used by porcelain and the CLI.
//!
//! Source for the library guide "Diff" page (included in the docs site).

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{unified_diff, DiffEntry, DiffStatus, TreeDiffOptions};
use grit_lib::repo::Repository;

use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    let mut opts = RepositoryOptions::from_process();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Ok(Repository::open(&git_dir, &opts)?)
}

fn print_name_status(entries: &[DiffEntry]) {
    for entry in entries {
        let letter = entry.status.letter();
        let path = entry.path();
        if entry.status == DiffStatus::Renamed {
            if let (Some(old), Some(new)) = (&entry.old_path, &entry.new_path) {
                println!("{letter}\t{old}\t{new}");
                continue;
            }
        }
        println!("{letter}\t{path}");
    }
}

fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow::anyhow!("missing repository path"))?;
    let repo = open_repo(&root)?;

    let head = repo.references().head()?;
    let new_commit = head
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no commit"))?;
    let new_commit_data = repo.objects().read_commit(&new_commit)?;
    let parent = new_commit_data
        .parents
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("need a commit with a parent"))?;

    let old_tree = repo.objects().commit_tree(&parent)?;
    let new_tree = new_commit_data.tree;

    let tree_changes = repo
        .diff()
        .tree_to_tree(
            Some(&old_tree),
            Some(&new_tree),
            &TreeDiffOptions::default(),
        )?
        .entries;
    println!("tree_diff_begin");
    print_name_status(&tree_changes);
    println!("tree_diff_end");

    if repo.work_tree().is_some() {
        let mut index = repo
            .index()
            .load(&grit_lib::index::IndexLoadOptions::default())?;
        let wt_changes = repo
            .diff()
            .index_to_worktree(&mut index, &TreeDiffOptions::default())?;
        println!("index_worktree_dirty={}", !wt_changes.entries.is_empty());
    }

    if let (Some(entry),) = (tree_changes.first(),) {
        if entry.status == DiffStatus::Modified {
            let old_blob = repo.odb().read(&entry.old_oid)?.data;
            let new_blob = repo.odb().read(&entry.new_oid)?.data;
            let old_text = String::from_utf8_lossy(&old_blob);
            let new_text = String::from_utf8_lossy(&new_blob);
            let old_path = entry.old_path.as_deref().unwrap_or(entry.path());
            let new_path = entry.new_path.as_deref().unwrap_or(entry.path());
            let patch = unified_diff(&old_text, &new_text, old_path, new_path, 3, false, false);
            let hunk_lines = patch
                .lines()
                .filter(|l| l.starts_with('-') || l.starts_with('+'))
                .count();
            println!("blob_unified_hunk_lines={hunk_lines}");
        }
    }

    Ok(())
}

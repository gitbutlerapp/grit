//! Tree-to-tree, index-to-worktree, and blob diffs used by porcelain and the CLI.
//!
//! Source for the library guide "Diff" page (included in the docs site).

use grit_lib::diff::{diff_index_to_worktree, diff_trees, unified_diff, DiffEntry, DiffStatus};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
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
    Repository::open(&git_dir, work_tree)
}

fn tree_oid(repo: &Repository, commit: &ObjectId) -> Result<ObjectId, grit_lib::error::Error> {
    let obj = repo.odb.read(commit)?;
    Ok(parse_commit(&obj.data)?.tree)
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

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let head = resolve_head(&repo.git_dir)?;
    let new_commit = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;
    let new_obj = repo.odb.read(&new_commit)?;
    let new_commit_data = parse_commit(&new_obj.data)?;
    let parent = new_commit_data
        .parents
        .first()
        .copied()
        .ok_or_else(|| grit_lib::error::Error::Message("need a commit with a parent".into()))?;

    let old_tree = tree_oid(&repo, &parent)?;
    let new_tree = new_commit_data.tree;

    let tree_changes = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "")?;
    println!("tree_diff_begin");
    print_name_status(&tree_changes);
    println!("tree_diff_end");

    if let Some(wt) = repo.work_tree.as_deref() {
        let mut index = repo.load_index()?;
        let wt_changes = diff_index_to_worktree(&repo.odb, &mut index, wt, false, false)?;
        println!("index_worktree_dirty={}", !wt_changes.is_empty());
    }

    if let (Some(entry),) = (tree_changes.first(),) {
        if entry.status == DiffStatus::Modified {
            let old_blob = repo.odb.read(&entry.old_oid)?.data;
            let new_blob = repo.odb.read(&entry.new_oid)?.data;
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

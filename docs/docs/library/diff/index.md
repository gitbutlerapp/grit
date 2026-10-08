# Diff

> Tree-to-tree and index-to-worktree diffs, blob patches, and the DiffEntry model the grit CLI renders.

Diffing in grit-lib centers on [`diff`](https://docs.rs/grit-lib/latest/grit_lib/diff/index.html) and the [`diffing`](https://docs.rs/grit-lib/latest/grit_lib/diffing/index.html) module view. Results are [`DiffEntry`](https://docs.rs/grit-lib/latest/grit_lib/diff/struct.DiffEntry.html) rows with a [`DiffStatus`](https://docs.rs/grit-lib/latest/grit_lib/diff/enum.DiffStatus.html) letter (`M`, `A`, `D`, …), paths, modes, and object ids — the same shape [`grit diff`](https://grit-scm.com/docs/diff/index.md) and [`porcelain::status`](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/index.html) use before formatting output.

## Tree-to-tree

[`diff_trees`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.diff_trees.html) compares two tree objects recursively and returns changed paths. Pass `None` for either side to diff against an empty tree. [`diff_trees_show_tree_entries`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.diff_trees_show_tree_entries.html) can emit tree objects themselves (Git’s `diff-tree -t` behavior).

Commit-to-commit diffs resolve each commit’s tree with [`parse_commit`](https://docs.rs/grit-lib/latest/grit_lib/objects/fn.parse_commit.html), then call `diff_trees` on the parent and child trees.

## Index-to-worktree

[`diff_index_to_worktree`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.diff_index_to_worktree.html) compares the index to files on disk. [`diff_index_to_worktree_with_options`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.diff_index_to_worktree_with_options.html) adds index mtime, submodule/gitlink handling, broken-gitlink detection, and an optional repository git-dir override. [`porcelain::add::stage`](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/fn.stage.html) uses [`diff_index_to_worktree_for_staging`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.diff_index_to_worktree_for_staging.html) internally so staging sees the same dirty paths as status.

## Blob diffs

For a single modified file, read old and new bytes from [`Odb`](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) and pass them to [`unified_diff`](https://docs.rs/grit-lib/latest/grit_lib/diff/fn.unified_diff.html) (histogram algorithm, Git-compatible hunks). The CLI builds human, `--json`, and `--markdown` views from `DiffEntry` lists plus optional unified bodies.

## Porcelain models

- **Status** — combines index vs HEAD and index vs worktree scans into structured sections (the CLI maps them to default / `--json` / `--markdown` output).
- **Add** — uses diff results to decide which paths to hash and stage.
- **Diff command** — tree-to-tree for commit ranges, index-to-worktree for uncommitted changes, then blob-level rendering for text files.

## Example

The program below diffs the latest commit against its parent (tree-to-tree), checks whether the work tree is dirty, and counts unified-diff lines for one modified blob:

```rust
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
```

Requires a repository whose `HEAD` has a parent (at least two commits):

```
cargo run --bin guide_diff /path/to/repo
git -C /path/to/repo diff --name-status HEAD~1 HEAD
```

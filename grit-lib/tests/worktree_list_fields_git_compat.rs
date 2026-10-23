//! `list_worktrees` reports current, lock, and prunable fields like `git worktree list --porcelain`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::git_path::path_for_disk_compare;
use grit_lib::repo::Repository;
use grit_lib::worktree::{list_worktrees, PrunableReason};
use grit_test_support::git;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
struct GitPorcelainRow {
    locked: bool,
    lock_reason: Option<String>,
    prunable_reason: Option<String>,
}

fn parse_git_worktree_porcelain(text: &str) -> HashMap<PathBuf, GitPorcelainRow> {
    let mut map = HashMap::new();
    let mut current_path: Option<PathBuf> = None;
    let mut row = GitPorcelainRow::default();
    for line in text.lines() {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                map.insert(path, row);
                row = GitPorcelainRow::default();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(path) = current_path.take() {
                map.insert(path, row);
                row = GitPorcelainRow::default();
            }
            current_path = Some(path_for_disk_compare(Path::new(rest)));
            continue;
        }
        if line == "locked" {
            row.locked = true;
            row.lock_reason = None;
            continue;
        }
        if let Some(reason) = line.strip_prefix("locked ") {
            row.locked = true;
            row.lock_reason = Some(reason.to_owned());
            continue;
        }
        if let Some(reason) = line.strip_prefix("prunable ") {
            row.prunable_reason = Some(reason.to_owned());
        }
    }
    if let Some(path) = current_path.take() {
        map.insert(path, row);
    }
    map
}

fn open_grit(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover")
}

fn assert_list_matches_git(open_from: &Path, main: &Path) {
    let git_rows = parse_git_worktree_porcelain(&git(main, &["worktree", "list", "--porcelain"]));
    let grit_list = list_worktrees(&open_grit(open_from)).expect("list");
    for entry in &grit_list {
        let git_row = git_rows
            .get(&entry.path)
            .unwrap_or_else(|| panic!("git missing path {}", entry.path.display()));
        assert_eq!(
            entry.is_locked,
            git_row.locked,
            "lock flag for {}",
            entry.path.display()
        );
        assert_eq!(
            entry.lock_reason.as_deref(),
            git_row.lock_reason.as_deref(),
            "lock reason for {}",
            entry.path.display()
        );
        let grit_prunable = entry
            .prunable_reason
            .map(PrunableReason::porcelain_message)
            .map(str::to_owned);
        assert_eq!(
            grit_prunable.as_deref(),
            git_row.prunable_reason.as_deref(),
            "prunable reason for {}",
            entry.path.display()
        );
        assert_eq!(
            entry.is_prunable,
            git_row.prunable_reason.is_some(),
            "is_prunable for {}",
            entry.path.display()
        );
    }

    assert_eq!(
        grit_list.iter().filter(|e| e.is_current).count(),
        1,
        "exactly one current worktree when opened from {}",
        open_from.display()
    );
    let open_path = path_for_disk_compare(open_from);
    let current = grit_list
        .iter()
        .find(|e| e.is_current)
        .expect("current row");
    assert_eq!(
        path_for_disk_compare(&current.path),
        open_path,
        "is_current must match opening path"
    );
}

#[test]
fn worktree_list_reports_current_lock_and_prunable_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).expect("mkdir");
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "Test"]);
    std::fs::write(main.join("f"), b"x\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "seed"]);

    let wt_locked_reason = tmp.path().join("wt-locked-reason");
    git(
        &main,
        &[
            "worktree",
            "add",
            wt_locked_reason.to_str().unwrap(),
            "-b",
            "locked-reason",
        ],
    );
    git(
        &main,
        &[
            "worktree",
            "lock",
            wt_locked_reason.to_str().unwrap(),
            "--reason",
            "hold",
        ],
    );

    let wt_locked_bare = tmp.path().join("wt-locked-bare");
    git(
        &main,
        &[
            "worktree",
            "add",
            wt_locked_bare.to_str().unwrap(),
            "-b",
            "locked-bare",
        ],
    );
    git(
        &main,
        &["worktree", "lock", wt_locked_bare.to_str().unwrap()],
    );

    let wt_gone = tmp.path().join("wt-gone");
    git(
        &main,
        &["worktree", "add", wt_gone.to_str().unwrap(), "-b", "gone"],
    );
    std::fs::remove_dir_all(&wt_gone).expect("remove linked tree");

    assert_list_matches_git(&main, &main);
    assert_list_matches_git(&wt_locked_reason, &main);
}

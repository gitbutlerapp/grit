//! Issue #2167: with `core.ignorecase=true` (Git's default on Windows and macOS), the
//! worktree scan behind status, diff and commit must resolve case-folded paths once per
//! directory, not once per tracked file, and still report the same changes as `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree, DiffStatus};
use grit_lib::porcelain::status::StatusOptions;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::test_support::counters::{
    reset_worktree_metadata_probe, worktree_path_resolve_probe_count,
};
use grit_test_support::git;

const TOP_DIRS: usize = 4;
const LEAF_DIRS_PER_TOP: usize = 4;
const FILES_PER_LEAF: usize = 70;
/// Parent directories of tracked files, plus their ancestors.
const DIRS: usize = TOP_DIRS + TOP_DIRS * LEAF_DIRS_PER_TOP;
const TRACKED: usize = TOP_DIRS * LEAF_DIRS_PER_TOP * FILES_PER_LEAF;

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.ignorecase", "true"]);
    for top in 0..TOP_DIRS {
        for leaf in 0..LEAF_DIRS_PER_TOP {
            let dir = root.join(format!("Top{top}/leaf{leaf}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..FILES_PER_LEAF {
                let path = dir.join(format!("File{f}.txt"));
                std::fs::write(&path, format!("{top} {leaf} {f}\n")).expect("write");
                // Older than the index, so stat-matched entries are not racily clean.
                filetime::set_file_mtime(
                    &path,
                    filetime::FileTime::from_unix_time(1_700_000_000, 0),
                )
                .expect("mtime");
            }
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn unstaged_name_status(repo: &Repository) -> Vec<String> {
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let mut lines: Vec<String> = repo
        .diff()
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff index to worktree")
        .iter()
        .map(|e| {
            let letter = match e.status {
                DiffStatus::Modified => "M",
                DiffStatus::Deleted => "D",
                other => panic!("unexpected status {other:?} for {}", e.path()),
            };
            format!("{letter}\t{}", e.path())
        })
        .collect();
    lines.sort();
    lines
}

fn git_name_status(root: &std::path::Path) -> Vec<String> {
    let out = git(root, &["diff", "--name-status"]);
    let mut lines: Vec<String> = out.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// Each directory may be resolved a few times (scan parent, symlinked-ancestor checks), but
/// never once per file in it.
fn assert_resolutions_per_directory(what: &str) {
    let resolved = worktree_path_resolve_probe_count();
    assert!(
        resolved <= 4 * DIRS,
        "{what}: expected ignorecase path resolutions per directory ({DIRS} dirs), \
         got {resolved} for {TRACKED} tracked files"
    );
}

#[test]
fn clean_ignorecase_scan_resolves_paths_per_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let probe = reset_worktree_metadata_probe(root);
    assert!(unstaged_name_status(&repo).is_empty(), "clean tree");
    assert_resolutions_per_directory("diff");
    drop(probe);

    let _probe = reset_worktree_metadata_probe(root);
    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    assert!(model.unstaged.is_empty(), "clean status");
    assert!(model.untracked.is_empty(), "no untracked files");
    assert_resolutions_per_directory("status");
}

#[test]
fn clean_ignorecase_commit_scan_resolves_paths_per_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let summary = repo
        .index()
        .stage_tracked_modifications(root)
        .expect("tracked scan");
    assert_eq!(summary.change_count(), 0, "clean tree");
    assert_resolutions_per_directory("commit staging scan");
}

#[test]
fn ignorecase_scan_matches_git_for_modified_and_deleted_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    std::fs::write(root.join("Top1/leaf2/File7.txt"), "changed contents\n").expect("modify");
    std::fs::remove_file(root.join("Top3/leaf0/File42.txt")).expect("delete");
    let repo = open_repo(root);

    let expected = vec![
        "D\tTop3/leaf0/File42.txt".to_owned(),
        "M\tTop1/leaf2/File7.txt".to_owned(),
    ];
    assert_eq!(git_name_status(root), expected, "git sees the edits");
    let _probe = reset_worktree_metadata_probe(root);
    assert_eq!(unstaged_name_status(&repo), expected);
    assert_resolutions_per_directory("dirty diff");
}

#[test]
fn ignorecase_scan_still_finds_case_folded_spellings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    // A case-only rename on disk; with ignorecase the index entry still names this file.
    std::fs::rename(
        root.join("Top0/leaf1/File3.txt"),
        root.join("Top0/leaf1/file3.txt"),
    )
    .expect("rename");
    std::fs::write(root.join("Top0/leaf1/file3.txt"), "edited after rename\n").expect("edit");
    std::fs::rename(root.join("Top2"), root.join("top2")).expect("rename dir");
    let repo = open_repo(root);

    assert_eq!(
        unstaged_name_status(&repo),
        vec!["M\tTop0/leaf1/File3.txt".to_owned()],
        "the case-folded file is modified, not deleted; the renamed directory is clean"
    );
}

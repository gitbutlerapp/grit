//! `skip_index_path` must apply together with `restrict_to_paths` on regular files.

use std::collections::BTreeSet;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn diff_paths_with_options(
    root: &std::path::Path,
    restrict: BTreeSet<String>,
    skip: Arc<dyn Fn(&[u8]) -> bool + Send + Sync>,
) -> BTreeSet<String> {
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let mut index = repo.index().load().expect("index");
    let index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let config = repo.config().snapshot().expect("config");
    let (entries, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        root,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            index_mtime,
            refresh_index_stat_in_pass: true,
            refresh_fsmonitor: false,
            for_status: true,
            config: Some(config),
            thread_policy: Some(repo.thread_policy()),
            restrict_to_paths: Some(Arc::new(restrict)),
            skip_index_path: Some(skip),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    entries
        .into_iter()
        .filter_map(|e| e.old_path.or(e.new_path))
        .collect()
}

#[test]
fn skip_index_path_applies_to_restricted_regular_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("keep.txt"), "keep v1\n").expect("write");
    std::fs::write(root.join("skip.txt"), "skip v1\n").expect("write");
    git(root, &["add", "keep.txt", "skip.txt"]);
    git(root, &["commit", "-qm", "base"]);
    std::fs::write(root.join("keep.txt"), "keep v2\n").expect("modify keep");
    std::fs::write(root.join("skip.txt"), "skip v2\n").expect("modify skip");

    let restrict: BTreeSet<String> = ["keep.txt", "skip.txt"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let skip = Arc::new(|path: &[u8]| path.ends_with(b"skip.txt"))
        as Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

    let paths = diff_paths_with_options(root, restrict.clone(), Arc::clone(&skip));
    assert_eq!(paths, BTreeSet::from(["keep.txt".to_owned()]));

    let skip_none = Arc::new(|_path: &[u8]| false) as Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;
    let all = diff_paths_with_options(root, restrict, skip_none);
    assert_eq!(
        all,
        BTreeSet::from(["keep.txt".to_owned(), "skip.txt".to_owned()])
    );
}

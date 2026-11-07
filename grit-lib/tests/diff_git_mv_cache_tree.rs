//! Regression: external `git mv` must not leave a stale cache-tree oid on grit diff.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{DiffDetail, DiffOptions, TreeDiffOptions};
use grit_lib::repo::{InitOptions, Repository};

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn grit_diff_stat_after_git_mv_does_not_read_stale_cache_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    Repository::init(root, &InitOptions::default()).expect("init");
    std::fs::write(root.join("f.txt"), b"line\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);
    git(root, &["mv", "f.txt", "g.txt"]);
    std::fs::write(root.join("g.txt"), b"line\nextra\n").expect("write");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = repo.references().head().expect("head");
    let head_tree =
        grit_lib::diff::tree_oid_of_commit(repo.odb(), head.oid().expect("oid")).expect("tree");
    let index = repo.index().load().expect("index");
    let work_tree = repo.work_tree().expect("worktree");
    let config = repo.config().snapshot().expect("config");
    let policy = repo.thread_policy();

    let raw = repo
        .diff()
        .tree_to_worktree(Some(&head_tree), &index, &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("tree to worktree");
    repo.diff()
        .files_for_entries(raw, DiffDetail::Stat, &DiffOptions::default())
        .expect("stat diff");

    let head_tree2 = head_tree;
    let index2 = repo.index().load().expect("index");
    let staged = repo
        .diff()
        .index_to_tree(&index2, Some(&head_tree2), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("index to tree");
    assert!(
        !staged.is_empty(),
        "expected staged changes after git mv, got empty (cache-tree fast path bug?)"
    );
}

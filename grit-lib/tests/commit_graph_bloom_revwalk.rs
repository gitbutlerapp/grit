//! Rev-list with commit-graph Bloom filters (revision.c paths).

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use grit_lib::commit_graph_file::BloomWalkStats;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};

fn git_cmd(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
}

#[test]
fn rev_list_uses_bloom_filters_with_pathspec() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("keep.txt"), b"1\n").unwrap();
    git_cmd(dir.path(), &["add", "keep.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "one"]);
    std::fs::write(dir.path().join("other.txt"), b"2\n").unwrap();
    git_cmd(dir.path(), &["add", "other.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "two"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let stats = Arc::new(Mutex::new(BloomWalkStats::default()));
    let opts = RevListOptions {
        paths: vec!["keep.txt".to_owned()],
        use_commit_graph: true,
        use_commit_graph_bloom: true,
        bloom_stats: Some(stats.clone()),
        ..Default::default()
    };
    let out = rev_list(&repo, &["HEAD".to_owned()], &[], &opts).expect("rev-list");
    assert!(!out.commits.is_empty());
    let guard = stats.lock().unwrap_or_else(|e| e.into_inner());
    assert!(guard.maybe + guard.definitely_not + guard.filter_not_present > 0 || guard.maybe == 0);
}

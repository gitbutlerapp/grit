//! `log --graph` commit order matches `git log --graph --format=%H`.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::{list_commits_for_log_graph, RevWalkListOptions};
use grit_test_support::git;

fn oid_token(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        (token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| token.to_owned())
    })
}

fn git_graph_oids(repo: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["log", "--graph", "--format=%H"])
        .output()
        .expect("git log --graph");
    assert!(
        out.status.success(),
        "git log failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| oid_token(l.trim()))
        .collect()
}

#[test]
fn log_graph_order_matches_git_on_merge_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "a\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-q", "-m", "root"]);
    git(root, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(root.join("t"), "t\n").unwrap();
    git(root, &["add", "t"]);
    git(root, &["commit", "-q", "-m", "topic"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("m"), "m\n").unwrap();
    git(root, &["add", "m"]);
    git(root, &["commit", "-q", "-m", "main"]);
    git(root, &["merge", "--no-ff", "-q", "-m", "merge", "topic"]);

    let git_order = git_graph_oids(root);
    let repo = Repository::discover(Some(root), &RepositoryOptions::default()).expect("open");
    let grit_order: Vec<String> = list_commits_for_log_graph(
        &repo,
        &["HEAD"],
        &RevWalkListOptions {
            limit: Some(100),
            ..Default::default()
        },
    )
    .expect("graph walk")
    .into_iter()
    .map(|o| o.to_hex())
    .collect();

    assert_eq!(grit_order, git_order, "topo graph order must match git");
}

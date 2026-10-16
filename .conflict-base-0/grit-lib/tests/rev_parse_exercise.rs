//! Exercise rev-parse resolution paths (success and failure) for line coverage.

mod support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use std::fs;

use grit_lib::revwalk::rev_parse::{
    resolve_revision, resolve_revision_as_commit, resolve_revision_for_checkout_guess,
    resolve_revision_for_range_end, resolve_revision_for_verify,
    resolve_revision_without_index_dwim,
};

use support::{build_commit_dag, commit_file, git, git_rev_parse_verify, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

#[test]
fn exercise_many_specs_on_deep_history() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let mut edge_labels: Vec<(String, String)> = Vec::new();
    for i in 1..=100 {
        let child = format!("c{i}");
        let parent = if i == 1 {
            "root".to_string()
        } else {
            format!("c{}", i - 1)
        };
        edge_labels.push((child, parent));
    }
    let edges: Vec<(&str, &str)> = edge_labels
        .iter()
        .map(|(c, p)| (c.as_str(), p.as_str()))
        .collect();
    build_commit_dag(wt, &edges, &[("c100", "refs/heads/main")]);
    let repo = open_grit(wt);
    for n in 0..100 {
        let spec = format!("HEAD~{n}");
        let _ = resolve_revision_for_verify(&repo, &spec);
        let _ = resolve_revision_without_index_dwim(&repo, &spec);
        let _ = resolve_revision_for_range_end(&repo, &spec);
        let _ = resolve_revision_as_commit(&repo, &spec);
        let _ = resolve_revision_for_checkout_guess(&repo, &spec, true);
        let _ = resolve_revision_for_checkout_guess(&repo, &spec, false);
    }
    for n in 1..=3 {
        let spec = format!("main^^{n}");
        let _ = resolve_revision_for_verify(&repo, &spec);
    }
    for spec in [
        "main^!",
        "main^@",
        "HEAD^{commit}",
        "HEAD^{tree}",
        "HEAD^{/history harness c100}",
        ":/harness",
        "main@{0}",
        "main@{1}",
        "main@{upstream}",
        "main@{u}",
        "main@{push}",
        "@",
        "@{0}",
    ] {
        let _ = resolve_revision_for_verify(&repo, spec);
        let _ = resolve_revision(&repo, spec);
    }
}

#[test]
fn head_tilde_matches_git_when_commit_graph_is_stale() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    for i in 0..15 {
        git(wt, &["commit", "--allow-empty", "-m", &format!("c{i}")]);
    }
    git(wt, &["commit-graph", "write", "--reachable"]);
    let tip_at_graph_write = git_rev_parse_verify(wt, "HEAD").expect("tip at graph write");
    for i in 15..25 {
        git(wt, &["commit", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let repo = open_grit(wt);
    let spec = "HEAD~20";
    let grit_oid = resolve_revision(&repo, spec).expect("stale graph");
    let git_oid = git_rev_parse_verify(wt, spec).expect("git");
    assert_eq!(grit_oid, git_oid);
    assert_ne!(
        grit_oid, tip_at_graph_write,
        "fixture must resolve through commits added after the graph was written"
    );
    let info = wt.join(".git/objects/info");
    assert!(
        info.join("commit-graph").is_file() || info.join("commit-graphs").is_dir(),
        "commit-graph must still exist on disk (stale, not absent)"
    );
}

#[test]
fn head_tilde_matches_git_when_commit_graph_is_missing() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    for i in 0..25 {
        git(wt, &["commit", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let repo = open_grit(wt);
    let spec = "HEAD~20";
    let grit_oid = resolve_revision(&repo, spec).expect("no graph");
    assert_eq!(grit_oid, git_rev_parse_verify(wt, spec).expect("git"));
    let graph_path = wt.join(".git/objects/info/commit-graph");
    let _ = fs::remove_file(&graph_path);
    let _ = fs::remove_dir_all(wt.join(".git/objects/info/commit-graphs"));
    assert_eq!(
        resolve_revision(&repo, spec).expect("after delete"),
        grit_oid
    );
}

#[test]
fn exercise_failure_specs_do_not_panic() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f", "1\n", "one");
    let repo = open_grit(wt);
    for spec in [
        "not-a-ref-xyz",
        "HEAD~999",
        "deadbeef",
        "main^{tag}",
        "main:missing/path",
        ":missing",
        "HEAD^9",
        "main..",
        "...main",
        "@{-99}",
        "main@{upstream}@{upstream}",
    ] {
        let _ = resolve_revision_for_verify(&repo, spec);
        let _ = resolve_revision(&repo, spec);
    }
}

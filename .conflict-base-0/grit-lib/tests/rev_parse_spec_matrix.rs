//! Broad rev-parse oracle matrix across fixtures (line coverage complement).

mod support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    peel_to_commit_for_merge_base, peel_to_tree, resolve_reflog_walk_log_ref,
    resolve_revision_as_commit_without_index_dwim, resolve_revision_for_commit_tree_tree,
    resolve_revision_for_patch_old_blob, resolve_revision_for_verify,
    try_peel_to_commit_for_merge_base,
};

use support::{
    build_commit_dag, commit_file, git, oracle_one, oracle_table, HistoryRepo, ResolveMode,
};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn linear_specs() -> Vec<&'static str> {
    vec![
        "HEAD",
        "main",
        "refs/heads/main",
        "main^",
        "main~1",
        "main~2",
        "HEAD~0",
        "HEAD^0",
        "HEAD^1",
        "@",
        "@^{commit}",
        "main^{commit}",
        "main^{}",
        "main^^{1}",
        "main^1",
        "main^2",
        "HEAD~3",
        "HEAD^^",
        "HEAD~1^",
        ":/history harness c",
    ]
}

#[test]
fn verify_specs_on_linear_merge_and_upstream_fixtures() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    git(wt, &["branch", "feature", "main~1"]);
    git(wt, &["branch", "side", "main~2"]);
    let repo = open_grit(wt);
    oracle_table(&repo, wt, ResolveMode::Verify, &linear_specs());
    for spec in ["feature", "side", "m", "HEAD^!", "HEAD^@"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }

    commit_file(wt, "upstream.txt", "u\n", "upstream");
    let bare = tempfile::tempdir().expect("bare");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    let repo = open_grit(wt);
    for spec in [
        "main@{upstream}",
        "main@{u}",
        "@{upstream}",
        "@{u}",
        "main@{push}",
        "@{push}",
        "origin/main",
        "refs/remotes/origin/main",
    ] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

#[test]
fn api_peel_and_reflog_helpers_on_linear_repo() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let repo = open_grit(wt);
    let head = resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let _ = peel_to_tree(&repo, head).expect("peel tree");
    let _ = peel_to_commit_for_merge_base(&repo, head).expect("peel commit");
    let _ = try_peel_to_commit_for_merge_base(&repo, head).expect("try peel");
    let _ = resolve_revision_as_commit_without_index_dwim(&repo, "main").expect("as commit");
    let _ = resolve_revision_for_commit_tree_tree(&repo, "main").expect("commit-tree");
    let _ = resolve_revision_for_patch_old_blob(&repo, "HEAD:missing").ok();
    let _ = resolve_reflog_walk_log_ref(&repo, "HEAD");
    for spec in ["main@{0}", "main@{1}", "HEAD@{0}"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

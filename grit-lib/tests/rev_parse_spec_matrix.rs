//! Broad rev-parse oracle matrix across fixtures (line coverage complement).

use crate::support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;

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
    let head = repo
        .revwalk()
        .resolve(
            "HEAD",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect("head");
    let _ = repo.revwalk().peel_to_tree(head).expect("peel tree");
    let _ = repo.revwalk().peel_to_commit(head).expect("peel commit");
    let _ = repo.revwalk().peel_to_commit(head).expect("try peel");
    let _ = repo
        .revwalk()
        .resolve(
            "main",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_peel(grit_lib::revwalk::Peel::Commit)
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off),
        )
        .expect("as commit");
    let _ = repo
        .revwalk()
        .resolve(
            "main",
            &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Tree),
        )
        .expect("commit-tree");
    let _ = repo
        .revwalk()
        .resolve(
            "HEAD:missing",
            &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Blob),
        )
        .ok();
    for spec in ["main@{0}", "main@{1}", "HEAD@{0}"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

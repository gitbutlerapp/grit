//! Smoke test for [`support::history_harness`].

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};

use support::{build_commit_dag, git_rev_list, HistoryRepo};

#[test]
fn history_harness_smoke_three_commit_dag_matches_grit_rev_list() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);

    let git_oids = git_rev_list(wt, &["--reverse", "main"]);
    assert_eq!(git_oids.len(), 3, "expected three commits on main");

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        reverse: true,
        ..RevListOptions::default()
    };
    let grit_result = rev_list(&grit_repo, &[String::from("main")], &[], &opts).expect("rev-list");
    let grit_oids = grit_result.commits;

    assert_eq!(
        grit_oids, git_oids,
        "grit rev-list should match git rev-list --reverse on a linear DAG"
    );
}

#[test]
fn history_harness_smoke_merge_commit_has_two_parents() {
    use support::git_rev_parse;

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "a"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let m = oids.get("m").expect("merge label");
    let p1 = git_rev_parse(wt, &format!("{}^1", m.to_hex())).expect("parent 1");
    let p2 = git_rev_parse(wt, &format!("{}^2", m.to_hex())).expect("parent 2");
    assert_eq!(p1, oids["b"]);
    assert_eq!(p2, oids["c"]);
}

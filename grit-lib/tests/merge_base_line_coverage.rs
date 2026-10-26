//! Extra merge-base API coverage (diff helpers, symmetric counts, branch base).

use crate::support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::{
    ahead_of_target_commits, ancestor_closure, branch_base_for_tip, count_symmetric_ahead_behind,
    is_ancestor_odb_walk, merge_base_for_diff_index, merge_base_for_diff_two_commits,
    merge_bases_octopus, resolve_commit_specs, walk_commits_reachable_excluding_ancestors_of,
    MergeBaseForDiffError, ReachableWalkLimit,
};

use support::{build_commit_dag, git_merge_base, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn t6010_oids(wt: &std::path::Path) -> (ObjectId, ObjectId, ObjectId) {
    let oids = build_commit_dag(
        wt,
        &[
            ("D", "E"),
            ("F", "E"),
            ("C", "D"),
            ("B", "C"),
            ("A", "B"),
            ("G", "B"),
            ("G", "E"),
            ("H", "A"),
            ("H", "F"),
        ],
        &[
            ("A", "refs/tags/A"),
            ("B", "refs/tags/B"),
            ("G", "refs/tags/G"),
            ("H", "refs/tags/H"),
        ],
    );
    (oids["G"], oids["H"], oids["B"])
}

#[test]
fn diff_merge_base_helpers_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let (g, h, b) = t6010_oids(wt);
    let repo = open_grit(wt);
    let mb = merge_base_for_diff_two_commits(&repo, g, h).expect("two-commit");
    assert_eq!(mb, b);
    let mb_idx = merge_base_for_diff_index(&repo, h, g).expect("index");
    assert_eq!(mb_idx, b);
    assert_eq!(
        merge_base_for_diff_two_commits(&repo, g, g).expect("same"),
        g
    );
}

#[test]
fn resolve_commit_specs_and_ancestor_closure() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let (g, h, b) = t6010_oids(wt);
    let repo = open_grit(wt);
    let specs = vec!["G".to_string(), "H".to_string()];
    let resolved = resolve_commit_specs(&repo, &specs).expect("resolve");
    assert_eq!(resolved, vec![g, h]);
    let g_anc = ancestor_closure(&repo, g).expect("closure");
    assert!(g_anc.contains(&b));
    assert!(is_ancestor_odb_walk(repo.odb(), b, h).expect("odb walk"));
}

#[test]
fn symmetric_ahead_behind_and_walk_limits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let a = oids["a"];
    let d = oids["d"];
    let repo = open_grit(wt);
    let (ahead, behind) = count_symmetric_ahead_behind(&repo, d, a).expect("symmetric");
    assert_eq!(ahead, 3);
    assert_eq!(behind, 0);
    let (total, retained) = ahead_of_target_commits(&repo, d, a, 2).expect("ahead");
    assert_eq!(total, 3);
    assert_eq!(retained.len(), 2);
    let (emitted, walked) = walk_commits_reachable_excluding_ancestors_of(
        &repo,
        &[d],
        &[a],
        ReachableWalkLimit::StopAfter(1),
    )
    .expect("walk");
    assert_eq!(emitted, 1);
    assert_eq!(walked.len(), 1);
}

#[test]
fn branch_base_for_tip_and_empty_octopus() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let (g, h, b) = t6010_oids(wt);
    let repo = open_grit(wt);
    let idx = branch_base_for_tip(&repo, h, &[b, g]).expect("branch base");
    assert!(idx.is_some());
    assert_eq!(
        merge_bases_octopus(&repo, &[]).expect("empty"),
        Vec::<ObjectId>::new()
    );
    assert_eq!(git_merge_base(wt, &["G", "H"]), vec![b]);
}

#[test]
fn merge_base_for_diff_errors_on_unrelated() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(wt, &[("x", "r1"), ("y", "r2")], &[]);
    let repo = open_grit(wt);
    let err = merge_base_for_diff_two_commits(&repo, oids["x"], oids["y"]).unwrap_err();
    assert!(matches!(err, MergeBaseForDiffError::None));
}

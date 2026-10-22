//! Merge-base oracle tests (upstream t6010 / t6099 subsets).

mod support;

use std::collections::HashMap;
use std::fs;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::{
    fork_point, independent_commits, is_ancestor, merge_bases_all, merge_bases_first_vs_rest,
    merge_bases_octopus, resolve_fork_point_reflog_ref,
};

use support::{
    build_commit_dag, commit_labeled_with_dates, git_merge_base, git_merge_base_is_ancestor,
    git_rev_parse_verify, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn setup_t6010(wt: &std::path::Path) -> (ObjectId, ObjectId, ObjectId) {
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
    let g = oids["G"];
    let h = oids["H"];
    let b = oids["B"];
    (g, h, b)
}

#[test]
fn t6010_merge_base_variants_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let (g, h, b) = setup_t6010(wt);
    let repo = open_grit(wt);

    let git_single = git_merge_base(wt, &["G", "H"]);
    assert_eq!(git_single, vec![b]);
    let grit_single = merge_bases_first_vs_rest(&repo, g, &[h]).expect("merge-base");
    assert_eq!(grit_single, git_single);

    let git_all = git_merge_base(wt, &["--all", "G", "H"]);
    let grit_all = merge_bases_all(&repo, &[g, h]).expect("all");
    assert_eq!(grit_all, git_all);

    let git_oct = git_merge_base(wt, &["--octopus", "G", "H"]);
    let grit_oct = merge_bases_octopus(&repo, &[g, h]).expect("octopus");
    assert_eq!(grit_oct, git_oct);

    assert_eq!(
        git_merge_base_is_ancestor(wt, "B", "H"),
        is_ancestor(&repo, b, h).expect("is-ancestor")
    );

    let git_indep = git_merge_base(wt, &["--independent", "H"]);
    let grit_indep = independent_commits(&repo, &[h]).expect("independent");
    assert_eq!(grit_indep, git_indep);

    let git_indep3 = git_merge_base(wt, &["--independent", "A", "H", "G"]);
    let a = git_rev_parse_verify(wt, "A").expect("A");
    let grit_indep3 = independent_commits(&repo, &[a, h, g]).expect("independent3");
    assert_eq!(grit_indep3, git_indep3);
}

#[test]
fn merge_base_identical_with_and_without_commit_graph() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let (g, h, _) = setup_t6010(wt);
    let repo = open_grit(wt);

    let without = merge_bases_first_vs_rest(&repo, g, &[h]).expect("without graph");

    git(wt, &["commit-graph", "write", "--reachable"]);
    let with = merge_bases_first_vs_rest(&repo, g, &[h]).expect("with graph");

    assert_eq!(without, with);
    assert_eq!(without, git_merge_base(wt, &["G", "H"]));

    let info_dir = wt.join(".git/objects/info");
    let _ = fs::remove_file(info_dir.join("commit-graph"));
    let _ = fs::remove_dir_all(info_dir.join("commit-graphs"));
    let after_stale = merge_bases_first_vs_rest(&repo, g, &[h]).expect("stale graph");
    assert_eq!(after_stale, without);
    assert_eq!(after_stale, git_merge_base(wt, &["G", "H"]));
}

fn git(wt: &std::path::Path, args: &[&str]) {
    support::git(wt, args);
}

fn empty_commit(wt: &std::path::Path, msg: &str) {
    git(wt, &["commit", "--allow-empty", "-m", msg]);
}

/// t6010 criss-cross with a child commit dated older than its parent (no commit-graph).
fn build_t6010_skewed_commit_dates(wt: &std::path::Path) -> HashMap<String, ObjectId> {
    let e = commit_labeled_with_dates(wt, "E", &[], "1000 +0000", "1000 +0000");
    let d = commit_labeled_with_dates(wt, "D", &[e], "2000 +0000", "2000 +0000");
    let f = commit_labeled_with_dates(wt, "F", &[e], "2000 +0000", "2000 +0000");
    let c = commit_labeled_with_dates(wt, "C", &[d], "3000 +0000", "3000 +0000");
    // Non-monotonic along B → C: B is newer in the walk heap but older by committer date.
    let b = commit_labeled_with_dates(wt, "B", &[c], "500 +0000", "500 +0000");
    let a = commit_labeled_with_dates(wt, "A", &[b], "4000 +0000", "4000 +0000");
    let g = commit_labeled_with_dates(wt, "G", &[b, e], "4500 +0000", "4500 +0000");
    let h = commit_labeled_with_dates(wt, "H", &[a, f], "5000 +0000", "5000 +0000");
    let oids = HashMap::from([
        ("A".to_owned(), a),
        ("B".to_owned(), b),
        ("G".to_owned(), g),
        ("H".to_owned(), h),
    ]);
    for (label, refname) in [
        ("A", "refs/tags/A"),
        ("B", "refs/tags/B"),
        ("G", "refs/tags/G"),
        ("H", "refs/tags/H"),
    ] {
        git(wt, &["update-ref", refname, &oids[label].to_hex()]);
    }
    oids
}

#[test]
fn merge_base_default_skewed_dates_without_commit_graph_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_t6010_skewed_commit_dates(wt);
    let g = oids["G"];
    let h = oids["H"];

    let graph_path = wt.join(".git/objects/info/commit-graph");
    assert!(!graph_path.exists(), "fixture must not use commit-graph");

    git(wt, &["config", "user.name", "Merge Base Test"]);
    git(wt, &["config", "user.email", "merge-base-test@example.com"]);

    let repo = open_grit(wt);
    let git_one = git_merge_base(wt, &["G", "H"]);
    let grit_one = merge_bases_first_vs_rest(&repo, g, &[h]).expect("default merge-base");
    assert_eq!(
        grit_one, git_one,
        "merge_bases_first_vs_rest vs git merge-base"
    );
    git(wt, &["update-ref", "refs/heads/main", &h.to_hex()]);
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let git_merge_tree = grit_test_support::git_cmd(&["merge-tree", "--write-tree", "HEAD", "G"])
        .in_dir(wt)
        .exec()
        .stdout
        .lines()
        .next()
        .expect("git merge-tree")
        .trim()
        .to_owned();

    let outcome =
        integrate_branch(&repo, "refs/heads/main", h, g, "G").expect("integrate_branch merge");
    let IntegrateBranchOutcome::Merged { oid: merge_oid, .. } = outcome else {
        panic!("expected clean merge, got {outcome:?}");
    };
    let grit_tree = {
        let obj = repo.odb().read(&merge_oid).expect("merge commit");
        grit_lib::objects::parse_commit(&obj.data)
            .expect("parse")
            .tree
            .to_hex()
    };
    assert_eq!(
        grit_tree, git_merge_tree,
        "integrate_branch merge tree must match git merge-tree"
    );
}

#[test]
fn merge_base_all_skewed_dates_without_commit_graph_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_t6010_skewed_commit_dates(wt);
    let g = oids["G"];
    let h = oids["H"];
    let repo = open_grit(wt);

    let graph_path = wt.join(".git/objects/info/commit-graph");
    assert!(!graph_path.exists(), "fixture must not use commit-graph");

    let git_all = git_merge_base(wt, &["--all", "G", "H"]);
    let grit_all = merge_bases_all(&repo, &[g, h]).expect("all");
    assert_eq!(grit_all, git_all);
    for oid in &grit_all {
        for other in &grit_all {
            if oid != other {
                assert!(
                    !is_ancestor(&repo, *other, *oid).expect("ancestor check"),
                    "merge-base --all must be mutually minimal: {other} should not reach {oid}"
                );
            }
        }
    }
}

#[test]
fn t6099_side_exhaustion_merge_base_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    empty_commit(wt, "C");
    git(wt, &["tag", "C"]);
    git(wt, &["branch", "d-chain"]);
    empty_commit(wt, "d1");
    empty_commit(wt, "B");
    git(wt, &["tag", "B"]);
    git(wt, &["checkout", "-b", "e-path", "C"]);
    empty_commit(wt, "e1");
    empty_commit(wt, "e2");
    git(wt, &["checkout", "-b", "f-path", "C"]);
    empty_commit(wt, "f1");
    empty_commit(wt, "f2");
    git(wt, &["checkout", "-b", "branch-A", "e-path"]);
    git(wt, &["merge", "--no-edit", "B"]);
    git(wt, &["checkout", "-b", "branch-X", "f-path"]);
    git(wt, &["merge", "--no-edit", "B"]);
    git(wt, &["tag", "A", "branch-A"]);
    git(wt, &["tag", "X", "branch-X"]);
    git(wt, &["commit-graph", "write", "--reachable"]);

    let repo = open_grit(wt);
    let a = git_rev_parse_verify(wt, "A").expect("A");
    let x = git_rev_parse_verify(wt, "X").expect("X");
    let b = git_rev_parse_verify(wt, "B").expect("B");

    let git_all = git_merge_base(wt, &["--all", "A", "X"]);
    let grit_all = merge_bases_all(&repo, &[a, x]).expect("all");
    assert_eq!(grit_all, git_all);
    assert_eq!(git_all, vec![b]);

    let git_one = git_merge_base(wt, &["A", "X"]);
    let grit_one = merge_bases_first_vs_rest(&repo, a, &[x]).expect("one");
    assert_eq!(grit_one, git_one);

    let graph_path = wt.join(".git/objects/info/commit-graph");
    let _ = fs::remove_file(&graph_path);
    let git_no_graph = git_merge_base(wt, &["--all", "A", "X"]);
    let grit_no_graph = merge_bases_all(&repo, &[a, x]).expect("all no graph");
    assert_eq!(grit_no_graph, git_no_graph);
}

#[test]
fn fork_point_reflog_matches_git_t6010_fixture() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["commit", "--allow-empty", "-m", "E"]);
    let e = git_rev_parse_verify(wt, "HEAD").expect("E");
    git(wt, &["tag", "E"]);
    git(wt, &["checkout", "-b", "base", &e.to_hex()]);
    let mut expect: Vec<ObjectId> = Vec::new();
    let mut derived: Vec<ObjectId> = Vec::new();
    for count in 1..=3 {
        git(
            wt,
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("Base commit #{count}"),
            ],
        );
        expect.push(git_rev_parse_verify(wt, "HEAD").expect("base tip"));
        git(wt, &["checkout", "-B", "derived"]);
        git(
            wt,
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("Derived #{count}"),
            ],
        );
        derived.push(git_rev_parse_verify(wt, "HEAD").expect("derived tip"));
        git(wt, &["checkout", "-B", "base", &e.to_hex()]);
    }
    let repo = open_grit(wt);
    for (idx, derived_tip) in derived.iter().enumerate() {
        let git_fp = git_merge_base(wt, &["--fork-point", "base", &derived_tip.to_hex()]);
        assert_eq!(git_fp, vec![expect[idx]]);
        let upstream = git_rev_parse_verify(wt, "base").expect("base");
        let grit_fp = fork_point(&repo, "base", upstream, *derived_tip).expect("grit fork-point");
        assert_eq!(grit_fp, expect[idx]);
    }
    git(wt, &["checkout", "derived"]);
    let git_default = git_merge_base(wt, &["--fork-point", "base"]);
    assert_eq!(git_default, vec![expect[2]]);
    let upstream = git_rev_parse_verify(wt, "base").expect("base");
    let head = git_rev_parse_verify(wt, "HEAD").expect("derived");
    let grit_default = fork_point(&repo, "base", upstream, head).expect("grit default");
    assert_eq!(grit_default, expect[2]);
    assert_eq!(
        resolve_fork_point_reflog_ref(&repo, "base"),
        "refs/heads/base"
    );
}

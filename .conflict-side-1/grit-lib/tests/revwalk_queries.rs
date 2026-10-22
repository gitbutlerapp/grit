//! Rev-walk query API vs system `git rev-list` / `git merge-base`.

mod support;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::{LeftRightCounts, MergeFilter, RefSelection, RevWalkListOptions};

use support::{
    assert_rev_list_commits_match_git, build_commit_dag, git, git_commit_graph_write,
    git_merge_base, git_merge_base_is_ancestor, git_rev_list, setup_path_simplify_history,
    HistoryRepo,
};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn git_rev_list_count(wt: &Path, args: &[&str]) -> usize {
    let mut argv = vec!["rev-list", "--count"];
    argv.extend(args);
    git(wt, &argv).trim().parse().expect("count")
}

fn grit_rev_list_oids(
    repo: &Repository,
    tokens: &[&str],
    opts: &RevWalkListOptions,
) -> Vec<ObjectId> {
    repo.rev_walk().commits(tokens, opts).expect("grit commits")
}

fn grit_rev_list_count(repo: &Repository, tokens: &[&str], opts: &RevWalkListOptions) -> usize {
    repo.rev_walk().count(tokens, opts).expect("grit count")
}

#[test]
fn revs_matches_git_rev_list() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c"), ("e", "d"), ("f", "e")],
        &[("f", "refs/heads/main"), ("c", "refs/tags/mid")],
    );
    git(wt, &["branch", "side", &oids["b"].to_hex()]);
    git(wt, &["checkout", "side"]);
    build_commit_dag(wt, &[("s1", "b"), ("s2", "s1")], &[]);
    git(wt, &["checkout", "main"]);

    let repo = open_grit(wt);
    let walk_opts = RevWalkListOptions::default();

    for (git_args, tokens) in [
        (["main"].as_slice(), ["main"].as_slice()),
        (["main..HEAD"].as_slice(), ["main..HEAD"].as_slice()),
        (["^mid", "main"].as_slice(), ["^mid", "main"].as_slice()),
        (["main...side"].as_slice(), ["main...side"].as_slice()),
    ] {
        let git_oids = git_rev_list(wt, git_args);
        let grit_oids = grit_rev_list_oids(&repo, tokens, &walk_opts);
        assert_eq!(
            grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            "range {:?}",
            tokens
        );
    }

    let grit = open_grit(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--first-parent", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            first_parent: true,
            ..RevListOptions::default()
        },
        "first-parent main",
    );

    setup_path_simplify_history(wt);
    let repo = open_grit(wt);
    let git_path = git_rev_list(wt, &["rw6012-l", "--", "content.txt"]);
    let grit_path = grit_rev_list_oids(
        &repo,
        &["rw6012-l"],
        &RevWalkListOptions {
            paths: vec!["content.txt".into()],
            ..RevWalkListOptions::default()
        },
    );
    assert_eq!(
        grit_path.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_path.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn count_matches_git_rev_list_count() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let opts = RevWalkListOptions::default();
    let tokens = ["main"];

    let git_n = git_rev_list_count(wt, &["main"]);
    assert_eq!(grit_rev_list_count(&repo, &tokens, &opts), git_n);

    git_commit_graph_write(wt);
    let git_with = git_rev_list_count(wt, &["main"]);
    assert_eq!(grit_rev_list_count(&repo, &tokens, &opts), git_with);
    assert_eq!(git_n, git_with);
}

#[test]
fn merge_bases_match_git_merge_base_all() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = criss_cross_fixture(wt);
    let repo = open_grit(wt);
    let walk = repo.rev_walk();

    let g = oids["G"];
    let h = oids["H"];
    let git_single = git_merge_base(wt, &["G", "H"]);
    assert_eq!(
        walk.merge_bases("G", "H", false).expect("single"),
        git_single
    );

    let git_all = git_merge_base(wt, &["--all", "G", "H"]);
    assert_eq!(walk.merge_bases("G", "H", true).expect("all"), git_all);
    let _ = (g, h);
}

#[test]
fn merge_bases_default_one_base_when_two_minimal_bases_exist() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("o", "base"),
            ("a1", "o"),
            ("b1", "o"),
            ("a2", "a1"),
            ("b2", "b1"),
            ("m1", "a2"),
            ("m1", "b2"),
            ("a3", "a2"),
            ("b3", "b2"),
            ("m2", "a3"),
            ("m2", "b3"),
            ("tip", "m1"),
            ("tip", "m2"),
        ],
        &[("tip", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let walk = repo.rev_walk();

    let m1 = oids["m1"].to_hex();
    let m2 = oids["m2"].to_hex();
    let git_default = git_merge_base(wt, &[m1.as_str(), m2.as_str()]);
    assert_eq!(git_default.len(), 1, "fixture must expose two --all bases");
    let git_all = git_merge_base(wt, &["--all", m1.as_str(), m2.as_str()]);
    assert!(
        git_all.len() >= 2,
        "expected at least two minimal merge bases, got {git_all:?}"
    );

    let grit_default = walk
        .merge_bases(m1.as_str(), m2.as_str(), false)
        .expect("default");
    assert_eq!(grit_default, git_default);
    assert_eq!(grit_default.len(), 1);

    let grit_all = walk
        .merge_bases(m1.as_str(), m2.as_str(), true)
        .expect("all");
    assert_eq!(grit_all, git_all);
}

#[test]
fn merge_bases_criss_cross_side_exhaustion_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    t6099_criss_cross(wt);
    let repo = open_grit(wt);
    let walk = repo.rev_walk();
    let git_all = git_merge_base(wt, &["--all", "A", "X"]);
    assert_eq!(
        walk.merge_bases("A", "X", true).expect("criss-cross"),
        git_all
    );
}

#[test]
fn is_ancestor_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = criss_cross_fixture(wt);
    let repo = open_grit(wt);
    let walk = repo.rev_walk();
    let b = oids["B"];
    let h = oids["H"];
    assert_eq!(
        git_merge_base_is_ancestor(wt, "B", "H"),
        walk.is_ancestor("B", "H").expect("ancestor")
    );
    assert_eq!(
        git_merge_base_is_ancestor(wt, "H", "B"),
        walk.is_ancestor("H", "B").expect("not ancestor")
    );
    let _ = (b, h);
}

fn criss_cross_fixture(wt: &Path) -> HashMap<String, ObjectId> {
    build_commit_dag(
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
    )
}

fn t6099_criss_cross(wt: &Path) {
    git(wt, &["commit", "--allow-empty", "-m", "C"]);
    git(wt, &["tag", "C"]);
    git(wt, &["branch", "d-chain"]);
    git(wt, &["commit", "--allow-empty", "-m", "d1"]);
    git(wt, &["commit", "--allow-empty", "-m", "B"]);
    git(wt, &["tag", "B"]);
    git(wt, &["checkout", "-b", "e-path", "C"]);
    git(wt, &["commit", "--allow-empty", "-m", "e1"]);
    git(wt, &["commit", "--allow-empty", "-m", "e2"]);
    git(wt, &["checkout", "-b", "f-path", "C"]);
    git(wt, &["commit", "--allow-empty", "-m", "f1"]);
    git(wt, &["commit", "--allow-empty", "-m", "f2"]);
    git(wt, &["checkout", "-b", "branch-A", "e-path"]);
    git(wt, &["merge", "--no-edit", "B"]);
    git(wt, &["checkout", "-b", "branch-X", "f-path"]);
    git(wt, &["merge", "--no-edit", "B"]);
    git(wt, &["tag", "A", "branch-A"]);
    git(wt, &["tag", "X", "branch-X"]);
    git(wt, &["commit-graph", "write", "--reachable"]);
    let graph_path = wt.join(".git/objects/info/commit-graph");
    let _ = fs::remove_file(&graph_path);
}

fn git_rev_list_left_right_count(wt: &Path, args: &[&str]) -> LeftRightCounts {
    let mut argv = vec!["rev-list", "--left-right", "--count"];
    for arg in args {
        argv.push(arg);
    }
    let out = git(wt, &argv);
    let parts: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "git left-right count: {:?}", parts);
    LeftRightCounts {
        left: parts[0].parse().expect("left"),
        right: parts[1].parse().expect("right"),
    }
}

fn setup_revwalk_filter_fixture(wt: &Path) -> HashMap<String, ObjectId> {
    let oids = build_commit_dag(
        wt,
        &[
            ("root", "base"),
            ("m1", "root"),
            ("m2", "root"),
            ("merge", "m1"),
            ("merge", "m2"),
            ("tip", "merge"),
        ],
        &[("tip", "refs/heads/main"), ("merge", "refs/tags/release")],
    );
    git(wt, &["branch", "feature/alpha"]);
    build_commit_dag(wt, &[("fa", "tip"), ("fb", "fa")], &[]);
    git(wt, &["checkout", "main"]);
    git(wt, &["branch", "feature/beta"]);
    build_commit_dag(wt, &[("ba", "tip")], &[]);
    git(wt, &["checkout", "main"]);
    let main_tip = oids["tip"].to_hex();
    fs::create_dir_all(wt.join(".git/refs/remotes/origin")).expect("remotes");
    fs::write(
        wt.join(".git/refs/remotes/origin/main"),
        format!("{main_tip}\n"),
    )
    .expect("origin/main");
    git(wt, &["config", "branch.main.remote", "origin"]);
    git(wt, &["config", "branch.main.merge", "refs/heads/main"]);
    oids
}

#[test]
fn skip_and_limit_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c"), ("e", "d"), ("f", "e")],
        &[("f", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        skip: 2,
        limit: Some(2),
        ..RevWalkListOptions::default()
    };
    let git_oids = git_rev_list(wt, &["--skip=2", "-n", "2", "main"]);
    let grit_oids = grit_rev_list_oids(&repo, &["main"], &opts);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn since_until_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let old = support::commit_labeled_with_dates(
        wt,
        "old",
        &[],
        "@1000000000 +0000",
        "@1000000000 +0000",
    );
    let mid = support::commit_labeled_with_dates(
        wt,
        "mid",
        &[old],
        "@1500000000 +0000",
        "@1500000000 +0000",
    );
    let new_tip = support::commit_labeled_with_dates(
        wt,
        "new",
        &[mid],
        "@2000000000 +0000",
        "@2000000000 +0000",
    );
    git(wt, &["update-ref", "refs/heads/main", &new_tip.to_hex()]);
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        since: Some(1_400_000_000),
        until: Some(1_900_000_000),
        ..RevWalkListOptions::default()
    };
    let git_oids = git_rev_list(wt, &["--since=1400000000", "--until=1900000000", "main"]);
    let grit_oids = grit_rev_list_oids(&repo, &["main"], &opts);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    let bounded = RevWalkListOptions {
        since: Some(1_400_000_000),
        limit: Some(1),
        ..RevWalkListOptions::default()
    };
    let git_bounded = git_rev_list(wt, &["--since=1400000000", "-n", "1", "main"]);
    let grit_bounded = grit_rev_list_oids(&repo, &["main"], &bounded);
    assert_eq!(
        grit_bounded
            .iter()
            .map(ObjectId::to_hex)
            .collect::<Vec<_>>(),
        git_bounded.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn merges_and_no_merges_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    setup_revwalk_filter_fixture(wt);
    let repo = open_grit(wt);

    for (merges, git_flag) in [
        (MergeFilter::OnlyMerges, "--merges"),
        (MergeFilter::NoMerges, "--no-merges"),
    ] {
        let opts = RevWalkListOptions {
            merges,
            ..RevWalkListOptions::default()
        };
        let git_oids = git_rev_list(wt, &[git_flag, "main"]);
        let grit_oids = grit_rev_list_oids(&repo, &["main"], &opts);
        assert_eq!(
            grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            "{git_flag}"
        );
    }
}

#[test]
fn reverse_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        reverse: true,
        ..RevWalkListOptions::default()
    };
    let git_oids = git_rev_list(wt, &["--reverse", "main"]);
    let grit_oids = grit_rev_list_oids(&repo, &["main"], &opts);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn ref_sets_all_branches_tags_remotes_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    setup_revwalk_filter_fixture(wt);
    let repo = open_grit(wt);

    let git_oids = git_rev_list(wt, &["--all"]);
    let grit_oids = grit_rev_list_oids(
        &repo,
        &[],
        &RevWalkListOptions {
            refs: vec![RefSelection::All],
            ..RevWalkListOptions::default()
        },
    );
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    let git_br = git_rev_list(wt, &["--branches"]);
    let grit_br = grit_rev_list_oids(
        &repo,
        &[],
        &RevWalkListOptions {
            refs: vec![RefSelection::Branches(None)],
            ..RevWalkListOptions::default()
        },
    );
    assert_eq!(
        grit_br.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_br.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    let git_tags = git_rev_list(wt, &["--tags"]);
    let grit_tags = grit_rev_list_oids(
        &repo,
        &[],
        &RevWalkListOptions {
            refs: vec![RefSelection::Tags(None)],
            ..RevWalkListOptions::default()
        },
    );
    assert_eq!(
        grit_tags.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_tags.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    let git_rem = git_rev_list(wt, &["--remotes"]);
    let grit_rem = grit_rev_list_oids(
        &repo,
        &[],
        &RevWalkListOptions {
            refs: vec![RefSelection::Remotes(None)],
            ..RevWalkListOptions::default()
        },
    );
    assert_eq!(
        grit_rem.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_rem.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn branches_glob_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    setup_revwalk_filter_fixture(wt);
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        refs: vec![RefSelection::Branches(Some("feature/*".into()))],
        ..RevWalkListOptions::default()
    };
    let git_oids = git_rev_list(wt, &["--branches=feature/*"]);
    let grit_oids = grit_rev_list_oids(&repo, &[], &opts);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn left_right_counts_no_merges_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("base", "root"),
            ("l1", "base"),
            ("l2", "l1"),
            ("r1", "base"),
            ("merge", "l2"),
            ("merge", "r1"),
            ("ltip", "merge"),
            ("rtip", "r1"),
        ],
        &[("ltip", "refs/heads/left"), ("rtip", "refs/heads/right")],
    );
    let _ = oids;
    let repo = open_grit(wt);
    let token = "left...right";
    let git_counts = git_rev_list_left_right_count(wt, &[token, "--no-merges"]);
    let grit_counts = repo
        .rev_walk()
        .left_right_counts(
            &[token],
            &RevWalkListOptions {
                merges: MergeFilter::NoMerges,
                ..RevWalkListOptions::default()
            },
        )
        .expect("no-merges counts");
    assert_eq!(grit_counts, git_counts);
}

#[test]
fn since_bounded_merge_dag_count_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let base = support::commit_labeled_with_dates(wt, "base", &[], "@100 +0000", "@100 +0000");
    let left = support::commit_labeled_with_dates(wt, "left", &[base], "@300 +0000", "@300 +0000");
    let right =
        support::commit_labeled_with_dates(wt, "right", &[base], "@400 +0000", "@400 +0000");
    let merge =
        support::commit_labeled_with_dates(wt, "merge", &[left, right], "@500 +0000", "@500 +0000");
    git(wt, &["update-ref", "refs/heads/main", &merge.to_hex()]);
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        since: Some(200),
        limit: Some(20),
        ..RevWalkListOptions::default()
    };
    let git_n = git_rev_list_count(
        wt,
        &["--since=1970-01-01 00:03:20 +0000", "-n", "20", "main"],
    );
    let grit_n = grit_rev_list_count(&repo, &["main"], &opts);
    assert_eq!(grit_n, git_n);
    assert_eq!(git_n, 3);
}

#[test]
fn since_skew_bounded_limit_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let old = support::commit_labeled_with_dates(
        wt,
        "old",
        &[],
        "@1000000000 +0000",
        "@1000000000 +0000",
    );
    let skewed = support::commit_labeled_with_dates(
        wt,
        "skewed",
        &[old],
        "@2000000000 +0000",
        "@2000000000 +0000",
    );
    git(wt, &["update-ref", "refs/heads/main", &skewed.to_hex()]);
    let repo = open_grit(wt);
    let opts = RevWalkListOptions {
        since: Some(1_500_000_000),
        limit: Some(2),
        ..RevWalkListOptions::default()
    };
    let git_oids = git_rev_list(wt, &["--since=1500000000", "-n", "2", "main"]);
    let grit_oids = grit_rev_list_oids(&repo, &["main"], &opts);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
}

#[test]
fn left_right_counts_match_git_rev_list_left_right_count() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    git(wt, &["branch", "side", &oids["b"].to_hex()]);
    git(wt, &["checkout", "side"]);
    build_commit_dag(wt, &[("s1", "b"), ("s2", "s1")], &[]);
    git(wt, &["checkout", "main"]);
    let repo = open_grit(wt);
    let walk = repo.rev_walk();
    let token = "main...side";
    let git_counts = git_rev_list_left_right_count(wt, &[token]);
    let grit_counts = walk
        .left_right_counts(&[token], &RevWalkListOptions::default())
        .expect("left/right counts");
    assert_eq!(grit_counts, git_counts);
}

#[test]
fn left_right_counts_first_parent_shapes_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let commit = |label: &str, parents: &[ObjectId], time: i64| {
        let date = format!("@{time} +0000");
        support::commit_labeled_with_dates(wt, label, parents, &date, &date)
    };

    // Trunk with increasing dates, a side branch forked from it, and a merge of the side
    // branch back into the trunk.
    let mut trunk = vec![commit("t0", &[], 1_000)];
    for i in 1..40 {
        let parent = trunk[i - 1];
        trunk.push(commit(&format!("t{i}"), &[parent], 1_000 + i as i64 * 10));
    }
    let mut side = trunk[12];
    for i in 0..5 {
        side = commit(&format!("s{i}"), &[side], 1_125 + i as i64);
    }
    let merge = commit("merge", &[trunk[39], side], 2_000);
    let after_merge = commit("after-merge", &[merge], 2_010);

    // Descendants dated before their ancestors (clock skew) and a run of equal dates.
    let mut skewed = trunk[20];
    for i in 0..6 {
        skewed = commit(&format!("skew{i}"), &[skewed], 500 - i as i64);
    }
    let mut flat = trunk[25];
    for i in 0..6 {
        flat = commit(&format!("flat{i}"), &[flat], 1_250);
    }

    let refs = [
        ("refs/heads/main", after_merge),
        ("refs/heads/trunk", trunk[39]),
        ("refs/heads/old", trunk[5]),
        ("refs/heads/side", side),
        ("refs/heads/skewed", skewed),
        ("refs/heads/flat", flat),
    ];
    for (name, oid) in refs {
        git(wt, &["update-ref", name, &oid.to_hex()]);
    }

    let repo = open_grit(wt);
    let walk = repo.rev_walk();
    for token in [
        "old...trunk",
        "trunk...old",
        "trunk...trunk",
        "trunk~30...trunk~2",
        "side...trunk",
        "trunk...side",
        "old...side",
        "main...old",
        "old...main",
        "main...side",
        "side...main",
        "skewed...trunk",
        "trunk~19...skewed",
        "skewed...trunk~19",
        "skewed~6...skewed",
        "flat...flat~6",
        "flat~6...flat",
        "flat...skewed",
        "skewed...side",
    ] {
        let git_counts = git_rev_list_left_right_count(wt, &[token]);
        let grit_counts = walk
            .left_right_counts(&[token], &RevWalkListOptions::default())
            .unwrap_or_else(|e| panic!("{token}: {e}"));
        assert_eq!(grit_counts, git_counts, "{token}");

        let git_fp = git_rev_list_left_right_count(wt, &[token, "--first-parent"]);
        let grit_fp = walk
            .left_right_counts(
                &[token],
                &RevWalkListOptions {
                    first_parent: true,
                    ..RevWalkListOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{token} --first-parent: {e}"));
        assert_eq!(grit_fp, git_fp, "{token} --first-parent");
    }
}

#[test]
fn upstream_range_at_u_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    setup_revwalk_filter_fixture(wt);
    let bare = wt.with_file_name("upstream-bare.git");
    let _ = fs::remove_dir_all(&bare);
    git(wt, &["init", "--bare", bare.to_str().expect("bare path")]);
    if git(wt, &["remote"]).trim().is_empty() {
        git(
            wt,
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
    }
    git(wt, &["push", "-q", "origin", "main"]);
    git(wt, &["fetch", "-q", "origin"]);
    git(wt, &["branch", "--set-upstream-to=origin/main", "main"]);
    let repo = open_grit(wt);
    let token = "@{u}...HEAD";
    let git_counts = git_rev_list_left_right_count(wt, &[token]);
    let grit_counts = repo
        .rev_walk()
        .left_right_counts(&[token], &RevWalkListOptions::default())
        .expect("upstream symmetric counts");
    assert_eq!(grit_counts, git_counts);
}

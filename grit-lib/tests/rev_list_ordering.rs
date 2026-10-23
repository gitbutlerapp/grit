//! Rev-list ordering, limiting, and range tests vs system `git` (history harness).

mod support;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_list::{
    split_revision_token, ObjectFilter, OrderingMode, OutputMode, RevListOptions,
};
use support::{
    assert_rev_list_commits_match_git, build_commit_dag, commit_file, commit_labeled_with_dates,
    expand_token_for_rev_list, git, git_commit_graph_write, git_merge_base_is_ancestor,
    git_rev_list, git_rev_list_lines, git_rev_parse, grit_rev_list_commits_in_repo, open_grit_repo,
    HistoryRepo,
};

fn topo_skew_dag(worktree: &Path) -> (ObjectId, HashMap<String, ObjectId>) {
    let oids = build_commit_dag(
        worktree,
        &[
            ("l1", "root"),
            ("l2", "l1"),
            ("a0", "l2"),
            ("a1", "a0"),
            ("b1", "a0"),
            ("b2", "b1"),
            ("c1", "b1"),
            ("c2", "c1"),
            ("c2", "b2"),
            ("a2", "a1"),
            ("a3", "a2"),
            ("b3", "b2"),
            ("b4", "b3"),
            ("b4", "a3"),
            ("a4", "a3"),
            ("a4", "b4"),
            ("a4", "c2"),
            ("l3", "a4"),
            ("l4", "l3"),
            ("l5", "l4"),
        ],
        &[("l5", "refs/heads/main"), ("root", "refs/tags/root")],
    );
    let _ = commit_labeled_with_dates(
        worktree,
        "b2a",
        &[oids["b1"]],
        "@1000000 +0000",
        "@2000000 +0000",
    );
    let tip = oids["l5"];
    (tip, oids)
}

fn criss_cross_dag(worktree: &Path) -> HashMap<String, ObjectId> {
    build_commit_dag(
        worktree,
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
    )
}

fn octopus_merge_dag(worktree: &Path) -> HashMap<String, ObjectId> {
    build_commit_dag(
        worktree,
        &[
            ("r", "base"),
            ("p1", "r"),
            ("p2", "r"),
            ("p3", "r"),
            ("oct", "p1"),
            ("oct", "p2"),
            ("oct", "p3"),
        ],
        &[("oct", "refs/heads/octopus")],
    )
}

fn dual_root_dag(worktree: &Path) -> HashMap<String, ObjectId> {
    build_commit_dag(
        worktree,
        &[("r1", "base1"), ("r2", "base2"), ("a", "r1"), ("b", "r2")],
        &[("a", "refs/heads/branch-a"), ("b", "refs/heads/branch-b")],
    )
}

fn cherry_symmetric_fixture(worktree: &Path) -> (String, String) {
    commit_file(worktree, "shared.txt", "origin\n", "seed");
    git(worktree, &["branch", "-M", "main"]);
    let base = git_rev_parse(worktree, "HEAD").expect("base");
    commit_file(worktree, "shared.txt", "same\n", "left-patch");
    git(worktree, &["checkout", "-b", "side", &base.to_hex()]);
    let _ = commit_file(worktree, "side.txt", "side\n", "side-only");
    let _ = commit_file(worktree, "shared.txt", "same\n", "right-patch");
    git(worktree, &["checkout", "main"]);
    ("main".to_owned(), "side".to_owned())
}

#[test]
fn t6003_topo_date_author_date_order_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (_tip, _oids) = topo_skew_dag(wt);
    let grit = open_grit_repo(wt);

    for (extra, ordering) in [
        (Vec::<&str>::new(), OrderingMode::Topo),
        (vec!["--date-order"], OrderingMode::DateOrderWalk),
        (vec!["--author-date-order"], OrderingMode::AuthorDateWalk),
    ] {
        for reverse in [false, true] {
            let mut git_args = vec!["--topo-order"];
            if extra.contains(&"--date-order") {
                git_args = vec!["--date-order"];
            }
            if extra.contains(&"--author-date-order") {
                git_args = vec!["--author-date-order"];
            }
            if reverse {
                git_args.push("--reverse");
            }
            git_args.push("main");
            let opts = RevListOptions {
                ordering,
                reverse,
                ..RevListOptions::default()
            };
            assert_rev_list_commits_match_git(
                wt,
                &grit,
                &git_args,
                &[String::from("main")],
                &[],
                opts,
                "t6003 topo/date/author-date",
            );
        }
    }
}

#[test]
fn t6013_reverse_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["commit", "--allow-empty", "-m", "one"]);
    git(wt, &["tag", "root"]);
    git(wt, &["commit", "--allow-empty", "-m", "two"]);
    git(wt, &["checkout", "-b", "side", "HEAD~1"]);
    git(wt, &["commit", "--allow-empty", "-m", "three"]);
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "-s", "ours", "side", "-m", "merge"]);
    git(wt, &["commit", "--allow-empty", "-m", "five"]);
    commit_file(wt, "foo", "x\n", "touch foo");

    let grit = open_grit_repo(wt);
    let git_args = ["--reverse", "main"];
    let opts = RevListOptions {
        reverse: true,
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &git_args,
        &[String::from("main")],
        &[],
        opts,
        "t6013 reverse",
    );
    let path_git_args = [
        "--reverse",
        "--parents",
        "--full-history",
        "main",
        "--",
        "foo",
    ];
    let path_opts = RevListOptions {
        reverse: true,
        full_history: true,
        output_mode: OutputMode::Parents,
        paths: vec!["foo".to_owned()],
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &path_git_args,
        &[String::from("main")],
        &[],
        path_opts.clone(),
        "t6013 reverse path-limited parents full-history",
    );

    let root_hex = git_rev_parse(wt, "root").expect("root tag").to_hex();
    let exclude_root = format!("^{root_hex}");
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[
            "--reverse",
            "--parents",
            "--full-history",
            "main",
            &exclude_root,
            "--",
            "foo",
        ],
        &[String::from("main")],
        &[root_hex.clone()],
        path_opts.clone(),
        "t6013 reverse path-limited ^root",
    );

    let git_boundary = support::git_rev_list(
        wt,
        &[
            "--boundary",
            "--reverse",
            "--parents",
            "--full-history",
            "main",
            &exclude_root,
            "--",
            "foo",
        ],
    );
    let boundary_opts = RevListOptions {
        reverse: true,
        full_history: true,
        boundary: true,
        output_mode: OutputMode::Parents,
        paths: vec!["foo".to_owned()],
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut opts = boundary_opts.clone();
        opts.use_commit_graph = use_graph;
        let result = grit_lib::revwalk::rev_list::rev_list(
            &grit,
            &[String::from("main")],
            &[root_hex.clone()],
            &opts,
        )
        .expect("boundary rev_list");
        let grit_seq: Vec<ObjectId> = if opts.reverse {
            result
                .boundary_commits
                .into_iter()
                .chain(result.commits)
                .collect()
        } else {
            let mut seq = result.commits;
            seq.extend(result.boundary_commits);
            seq
        };
        assert_eq!(
            grit_seq, git_boundary,
            "t6013 boundary reverse path ^root (use_commit_graph={use_graph})"
        );
    }
}

fn max_count_boundary_path_fixture(worktree: &Path) -> String {
    commit_file(worktree, "foo", "root\n", "root touch foo");
    git(worktree, &["tag", "root"]);
    git(worktree, &["commit", "--allow-empty", "-m", "first-parent"]);
    git(worktree, &["checkout", "-b", "side", "root"]);
    git(worktree, &["commit", "--allow-empty", "-m", "side"]);
    git(worktree, &["checkout", "main"]);
    git(worktree, &["merge", "-s", "ours", "side", "-m", "merge"]);
    commit_file(worktree, "foo", "tip\n", "tip touch foo");
    git_rev_parse(worktree, "root").expect("root tag").to_hex()
}

#[test]
fn t6013_max_count_boundary_path_limited_parents_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let root_hex = max_count_boundary_path_fixture(wt);
    let grit = open_grit_repo(wt);
    let exclude_root = format!("^{root_hex}");
    let git_args = [
        "--max-count=1",
        "--boundary",
        "--parents",
        "--full-history",
        "main",
        &exclude_root,
        "--",
        "foo",
    ];
    let git_oids = support::git_rev_list(wt, &git_args);
    let opts = RevListOptions {
        max_count: Some(1),
        boundary: true,
        full_history: true,
        output_mode: OutputMode::Parents,
        paths: vec!["foo".to_owned()],
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut run_opts = opts.clone();
        run_opts.use_commit_graph = use_graph;
        let result = grit_lib::revwalk::rev_list::rev_list(
            &grit,
            &[String::from("main")],
            &[root_hex.clone()],
            &run_opts,
        )
        .expect("max-count boundary rev_list");
        let mut grit_seq = result.commits;
        grit_seq.extend(result.boundary_commits);
        assert_eq!(
            grit_seq, git_oids,
            "t6013 max-count boundary path ^root (use_commit_graph={use_graph})"
        );
    }
}

#[test]
fn t6005_max_count_skip_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c"), ("e", "d")],
        &[("e", "refs/heads/main")],
    );
    let grit = open_grit_repo(wt);

    for (max, skip) in [(Some(2usize), 0usize), (Some(3), 1), (None, 2)] {
        let mut git_args: Vec<String> = vec!["main".to_owned()];
        if let Some(n) = max {
            git_args.insert(0, n.to_string());
            git_args.insert(0, "--max-count".to_owned());
        }
        if skip > 0 {
            git_args.insert(0, skip.to_string());
            git_args.insert(0, "--skip".to_owned());
        }
        let git_arg_refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let opts = RevListOptions {
            max_count: max,
            skip,
            ..RevListOptions::default()
        };
        assert_rev_list_commits_match_git(
            wt,
            &grit,
            &git_arg_refs,
            &[String::from("main")],
            &[],
            opts,
            "t6005 max-count/skip",
        );
    }

    let count_git: usize = git(wt, &["rev-list", "--count", "main"])
        .trim()
        .parse()
        .expect("count");
    let opts = RevListOptions {
        count: true,
        ..RevListOptions::default()
    };
    let grit_count =
        grit_lib::revwalk::rev_list::rev_list(&grit, &[String::from("main")], &[], &opts)
            .expect("count")
            .commit_count();
    assert_eq!(grit_count, count_git);
}

#[test]
fn t6009_first_parent_min_max_parents_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(
        wt,
        &[
            ("b", "a"),
            ("c", "b"),
            ("side", "b"),
            ("m", "c"),
            ("m", "side"),
            ("tip", "m"),
        ],
        &[("tip", "refs/heads/main")],
    );
    let grit = open_grit_repo(wt);

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
        "t6009 first-parent",
    );

    octopus_merge_dag(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--min-parents=3", "octopus"],
        &[String::from("octopus")],
        &[],
        RevListOptions {
            min_parents: Some(3),
            ..RevListOptions::default()
        },
        "t6009 min-parents octopus",
    );

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--max-parents=1", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            max_parents: Some(1),
            ..RevListOptions::default()
        },
        "t6009 max-parents",
    );
}

#[test]
fn t6019_ancestry_path_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("b", "a"),
            ("c", "b"),
            ("d", "c"),
            ("e", "d"),
            ("f", "e"),
            ("g", "c"),
            ("h", "e"),
            ("h", "g"),
            ("i", "h"),
            ("j", "f"),
            ("j", "i"),
            ("k", "a"),
            ("l", "j"),
            ("l", "k"),
            ("m", "l"),
        ],
        &[("d", "refs/heads/d"), ("m", "refs/heads/main")],
    );
    let grit = open_grit_repo(wt);
    let d = oids["d"].to_hex();
    let m = oids["m"].to_hex();
    let range = format!("{d}..{m}");
    let git_args = ["--ancestry-path", &range];
    let (pos, neg) = split_revision_token(&range);
    let opts = RevListOptions {
        ancestry_path: true,
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &git_args,
        &pos,
        &neg,
        opts,
        "t6019 ancestry-path",
    );
}

#[test]
fn t6100_in_order_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    for name in ["one", "two", "three", "four"] {
        commit_file(wt, name, &format!("{name}\n"), &format!("add {name}"));
    }
    for name in ["four", "three"] {
        git(wt, &["rm", name]);
        git(wt, &["commit", "-m", &format!("remove {name}")]);
    }
    let grit = open_grit_repo(wt);

    let git_raw = git_rev_list_lines(wt, &["--in-commit-order", "--objects", "HEAD"]);
    let git_oids: Vec<ObjectId> = git_raw
        .iter()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect();

    let opts = RevListOptions {
        objects: true,
        in_commit_order: true,
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut opts = opts.clone();
        opts.use_commit_graph = use_graph;
        let result =
            grit_lib::revwalk::rev_list::rev_list(&grit, &[String::from("HEAD")], &[], &opts)
                .expect("in-order walk");
        let mut grit_oids = Vec::new();
        for (i, commit) in result.commits.iter().enumerate() {
            grit_oids.push(*commit);
            if let Some(count) = result.per_commit_object_counts.get(i) {
                let start: usize = result.per_commit_object_counts[..i].iter().sum();
                for (oid, _) in &result.objects[start..start + count] {
                    grit_oids.push(*oid);
                }
            }
        }
        assert_eq!(
            grit_oids, git_oids,
            "t6100 in-commit-order (graph={use_graph})"
        );
    }
}

#[test]
fn t6001_grafts_and_replace_refs_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let a0 = commit_file(wt, "fileA", "A\n", "A0");
    let _a1 = commit_file(wt, "fileA", "A2\n", "A1");
    let a2 = commit_file(wt, "fileA", "A3\n", "A2");
    git(wt, &["update-ref", "-d", "refs/heads/main"]);
    let _b0 = commit_file(wt, "fileA", "B\n", "B0");
    let b2 = commit_file(wt, "fileA", "B3\n", "B2");
    git(wt, &["update-ref", "refs/heads/main", &b2.to_hex()]);

    let grafts_dir = wt.join(".git/info");
    fs::create_dir_all(&grafts_dir).expect("info dir");
    fs::write(
        grafts_dir.join("grafts"),
        format!("{} {}\n", b2.to_hex(), a0.to_hex()),
    )
    .expect("grafts");

    let grit = open_grit_repo(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["main"],
        &[String::from("main")],
        &[],
        RevListOptions::default(),
        "t6001 graft parents",
    );

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--parents", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            output_mode: OutputMode::Parents,
            ..RevListOptions::default()
        },
        "t6001 graft --parents",
    );

    let replaced = commit_file(wt, "fileA", "graft-replace\n", "replace");
    git(wt, &["replace", "-f", &a2.to_hex(), &replaced.to_hex()]);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["main"],
        &[String::from("main")],
        &[],
        RevListOptions::default(),
        "t6001 replace ref",
    );
}

#[test]
fn symmetric_diff_left_right_cherry_mark_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main_ref, side_ref) = cherry_symmetric_fixture(wt);
    let grit = open_grit_repo(wt);
    let token = format!("{main_ref}...{side_ref}");
    let mut opts = RevListOptions {
        left_right: true,
        left_right_explicit: true,
        cherry_mark: true,
        ..RevListOptions::default()
    };
    let (pos, neg) = expand_token_for_rev_list(&grit, &token, &mut opts).expect("symmetric");
    let git_args = ["--left-right", "--cherry-mark", &token];
    let git_lines = git_rev_list_lines(wt, &git_args);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--left-right", "--cherry-mark", &token],
        &pos,
        &neg,
        opts.clone(),
        "symmetric cherry-mark order",
    );

    let result = grit_lib::revwalk::rev_list::rev_list(&grit, &pos, &neg, &opts).expect("cherry");
    for line in git_lines {
        let stripped = line.trim_start_matches(['<', '>', '=', ' '].as_ref());
        let oid: ObjectId = stripped.split_whitespace().next().unwrap().parse().unwrap();
        if line.starts_with('=') {
            assert!(
                result.cherry_equivalent.contains(&oid),
                "expected cherry equivalent {oid}"
            );
        }
    }
}

#[test]
fn t6000_misc_ranges_and_boundary_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main"), ("b", "refs/heads/other")],
    );
    let grit = open_grit_repo(wt);
    let range = format!("other..main");
    let (pos, neg) = split_revision_token(&range);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[&range],
        &pos,
        &neg,
        RevListOptions::default(),
        "t6000 A..B",
    );

    let neg_only = format!("^{}", oids["b"].to_hex());
    let (_pos2, neg2) = split_revision_token(&neg_only);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["main", &neg_only],
        &[String::from("main")],
        &neg2,
        RevListOptions::default(),
        "t6000 ^A",
    );

    git(wt, &["tag", "root", &oids["a"].to_hex()]);
    let opts = RevListOptions {
        boundary: true,
        ..RevListOptions::default()
    };
    let git_boundary = support::git_rev_list(wt, &["--boundary", "main", "^root"]);
    for use_graph in [false, true] {
        if use_graph {
            support::git_commit_graph_write(wt);
        }
        let mut opts = opts.clone();
        opts.use_commit_graph = use_graph;
        let result = grit_lib::revwalk::rev_list::rev_list(
            &grit,
            &[String::from("main")],
            &[String::from("root")],
            &opts,
        )
        .expect("boundary walk");
        let mut grit_seq = result.commits;
        grit_seq.extend(result.boundary_commits);
        assert_eq!(grit_seq, git_boundary, "t6000 boundary (graph={use_graph})");
    }
}

#[test]
fn t6014_rev_list_all_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    dual_root_dag(wt);
    let grit = open_grit_repo(wt);
    let opts = RevListOptions {
        all_refs: true,
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(wt, &grit, &["--all"], &[], &[], opts, "t6014 --all");
}

#[test]
fn t4217_log_limit_since_until_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let old = commit_labeled_with_dates(wt, "old", &[], "@1000000000 +0000", "@1000000000 +0000");
    let mid =
        commit_labeled_with_dates(wt, "mid", &[old], "@1500000000 +0000", "@1500000000 +0000");
    let new_tip =
        commit_labeled_with_dates(wt, "new", &[mid], "@2000000000 +0000", "@2000000000 +0000");
    git(wt, &["update-ref", "refs/heads/main", &new_tip.to_hex()]);
    let grit = open_grit_repo(wt);

    let opts = RevListOptions {
        since_cutoff: Some(1_400_000_000),
        until_cutoff: Some(1_900_000_000),
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--since=1400000000", "--until=1900000000", "main"],
        &[String::from("main")],
        &[],
        opts,
        "t4217 since/until",
    );
}

#[test]
fn t4217_since_with_large_max_count_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let old = commit_labeled_with_dates(wt, "old", &[], "@1000000000 +0000", "@1000000000 +0000");
    let mid =
        commit_labeled_with_dates(wt, "mid", &[old], "@1500000000 +0000", "@1500000000 +0000");
    let new_tip =
        commit_labeled_with_dates(wt, "new", &[mid], "@2000000000 +0000", "@2000000000 +0000");
    git(wt, &["update-ref", "refs/heads/main", &new_tip.to_hex()]);
    let grit = open_grit_repo(wt);

    let opts = RevListOptions {
        since_cutoff: Some(1_400_000_000),
        max_count: Some(100_000),
        ..RevListOptions::default()
    };
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--since=1400000000", "-n", "100000", "main"],
        &[String::from("main")],
        &[],
        opts,
        "t4217 since with large -n",
    );
}

/// Linear 2001 → 2002 → 2003 commits: `--boundary` must retain parents dropped only by date cutoffs.
#[test]
fn t4217_boundary_since_until_match_git() {
    const CUTOFF: i64 = 1_022_803_200; // 2002-06-01 00:00:00 UTC

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let y2001 = commit_labeled_with_dates(wt, "y2001", &[], "@978307200 +0000", "@978307200 +0000");
    let y2002 = commit_labeled_with_dates(
        wt,
        "y2002",
        &[y2001],
        "@1009843200 +0000",
        "@1009843200 +0000",
    );
    let y2003 = commit_labeled_with_dates(
        wt,
        "y2003",
        &[y2002],
        "@1041379200 +0000",
        "@1041379200 +0000",
    );
    git(wt, &["update-ref", "refs/heads/main", &y2003.to_hex()]);
    let grit = open_grit_repo(wt);

    let since_git = format!("--since={CUTOFF}");
    let git_since_boundary = git_rev_list(wt, &["--boundary", since_git.as_str(), "main"]);
    let since_opts = RevListOptions {
        boundary: true,
        since_cutoff: Some(CUTOFF),
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut opts = since_opts.clone();
        opts.use_commit_graph = use_graph;
        let result =
            grit_lib::revwalk::rev_list::rev_list(&grit, &[String::from("main")], &[], &opts)
                .expect("since boundary rev_list");
        let mut grit_seq = result.commits;
        grit_seq.extend(result.boundary_commits);
        assert_eq!(
            grit_seq, git_since_boundary,
            "t4217 --boundary --since (use_commit_graph={use_graph})"
        );
    }

    let until_git = format!("--until={CUTOFF}");
    let git_until_boundary = git_rev_list(wt, &["--boundary", until_git.as_str(), "main"]);
    let until_opts = RevListOptions {
        boundary: true,
        until_cutoff: Some(CUTOFF),
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut opts = until_opts.clone();
        opts.use_commit_graph = use_graph;
        let result =
            grit_lib::revwalk::rev_list::rev_list(&grit, &[String::from("main")], &[], &opts)
                .expect("until boundary rev_list");
        let mut grit_seq = result.commits;
        grit_seq.extend(result.boundary_commits);
        assert_eq!(
            grit_seq, git_until_boundary,
            "t4217 --boundary --until (use_commit_graph={use_graph})"
        );
    }
}

/// Root and tip decorated refs: `--simplify-by-decoration` drops the middle; `--boundary` must not resurrect it.
#[test]
fn simplify_by_decoration_boundary_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "root\n", "root");
    git(wt, &["tag", "root"]);
    commit_file(wt, "f", "mid\n", "middle");
    commit_file(wt, "f", "tip\n", "tip");
    let grit = open_grit_repo(wt);
    let root = git_rev_parse(wt, "root").expect("root tag");
    let tip = git_rev_parse(wt, "main").expect("main tip");

    let git_boundary = git_rev_list(wt, &["--boundary", "--simplify-by-decoration", "main"]);
    let mut decorated = HashSet::new();
    decorated.insert(root);
    decorated.insert(tip);
    let opts = RevListOptions {
        boundary: true,
        simplify_by_decoration: true,
        simplify_by_decoration_oids: decorated,
        ..RevListOptions::default()
    };

    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let mut walk_opts = opts.clone();
        walk_opts.use_commit_graph = use_graph;
        let result =
            grit_lib::revwalk::rev_list::rev_list(&grit, &[String::from("main")], &[], &walk_opts)
                .expect("simplify-by-decoration boundary rev_list");
        let mut grit_seq = result.commits;
        grit_seq.extend(result.boundary_commits);
        assert_eq!(
            grit_seq, git_boundary,
            "simplify-by-decoration --boundary (use_commit_graph={use_graph})"
        );
    }
}

#[test]
fn ordering_identical_with_and_without_commit_graph() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    criss_cross_dag(wt);
    let grit = open_grit_repo(wt);
    for ordering in [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
    ] {
        let git_args = match ordering {
            OrderingMode::Topo => vec!["--topo-order", "main"],
            OrderingMode::DateOrderWalk => vec!["--date-order", "main"],
            _ => vec!["main"],
        };
        let opts = RevListOptions {
            ordering,
            ..RevListOptions::default()
        };
        assert_rev_list_commits_match_git(
            wt,
            &grit,
            &git_args,
            &[String::from("main")],
            &[],
            opts,
            "commit-graph parity",
        );
    }
}

#[test]
fn skewed_and_equal_commit_dates_order_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let root = commit_labeled_with_dates(wt, "root", &[], "@10 +0000", "@10 +0000");
    let skew = commit_labeled_with_dates(wt, "skew", &[root], "@100 +0000", "@500 +0000");
    let tip = commit_labeled_with_dates(wt, "equal", &[skew], "@300 +0000", "@300 +0000");
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let _ = skew;
    let grit = open_grit_repo(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--author-date-order", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            ordering: OrderingMode::AuthorDateWalk,
            ..RevListOptions::default()
        },
        "skewed author dates",
    );
}

#[test]
fn multiple_roots_and_octopus_merge_order_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    octopus_merge_dag(wt);
    dual_root_dag(wt);
    let grit = open_grit_repo(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--topo-order", "octopus"],
        &[String::from("octopus")],
        &[],
        RevListOptions {
            ordering: OrderingMode::Topo,
            ..RevListOptions::default()
        },
        "octopus topo",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["branch-a", "branch-b"],
        &[String::from("branch-a"), String::from("branch-b")],
        &[],
        RevListOptions::default(),
        "multiple roots",
    );
}

fn rich_history_repo() -> (HistoryRepo, ObjectId, ObjectId, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let mut labels: Vec<String> = (1..=40).map(|i| format!("c{i}")).collect();
    labels.extend(
        [
            "base", "side1", "side2", "side3", "merge1", "merge2", "merge3", "tip",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    let mut edges: Vec<(String, String)> = vec![("c1".to_owned(), "base".to_owned())];
    for i in 2..=40 {
        edges.push((format!("c{i}"), format!("c{}", i - 1)));
    }
    edges.extend([
        ("side1".to_owned(), "c20".to_owned()),
        ("side2".to_owned(), "c20".to_owned()),
        ("side3".to_owned(), "c20".to_owned()),
        ("merge1".to_owned(), "c40".to_owned()),
        ("merge1".to_owned(), "side1".to_owned()),
        ("merge2".to_owned(), "merge1".to_owned()),
        ("merge2".to_owned(), "side2".to_owned()),
        ("merge3".to_owned(), "merge2".to_owned()),
        ("merge3".to_owned(), "side3".to_owned()),
        ("tip".to_owned(), "merge3".to_owned()),
    ]);
    let edge_refs: Vec<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let oids = build_commit_dag(
        wt,
        &edge_refs,
        &[
            ("tip", "refs/heads/main"),
            ("c20", "refs/heads/stable"),
            ("base", "refs/tags/root"),
        ],
    );
    (repo, oids["base"], oids["c20"], oids["tip"])
}

#[test]
fn rev_list_broad_ordering_and_limit_flags_match_git() {
    let (repo, base, mid, tip) = rich_history_repo();
    let wt = repo.worktree();
    let grit = open_grit_repo(wt);
    let tip_hex = tip.to_hex();
    let mid_hex = mid.to_hex();
    let base_hex = base.to_hex();
    let range = format!("{mid_hex}..{tip_hex}");

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--merges", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            min_parents: Some(2),
            ..RevListOptions::default()
        },
        "merges-only",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--reverse", "--topo-order", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            reverse: true,
            ordering: OrderingMode::Topo,
            ..RevListOptions::default()
        },
        "reverse topo",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--max-count=5", "--skip=2", "--topo-order", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            max_count: Some(5),
            skip: 2,
            ordering: OrderingMode::Topo,
            ..RevListOptions::default()
        },
        "topo skip max",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--first-parent", "--max-count=10", "main"],
        &[String::from("main")],
        &[],
        RevListOptions {
            first_parent: true,
            max_count: Some(10),
            ..RevListOptions::default()
        },
        "first-parent max",
    );

    let git_count: usize = git(wt, &["rev-list", "--count", "main"])
        .trim()
        .parse()
        .expect("git count");
    let grit_count = grit_lib::revwalk::rev_list::rev_list(
        &grit,
        &[String::from("main")],
        &[],
        &RevListOptions {
            count: true,
            ..RevListOptions::default()
        },
    )
    .expect("grit count")
    .commit_count();
    assert_eq!(grit_count, git_count);

    let (pos, neg) = split_revision_token(&range);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[&range],
        &pos,
        &neg,
        RevListOptions::default(),
        "two-dot range",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["main", &format!("^{base_hex}")],
        &[String::from("main")],
        &[base_hex],
        RevListOptions::default(),
        "exclude base",
    );
}

#[test]
fn symmetric_cherry_pick_and_left_right_filters_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main_ref, side_ref) = cherry_symmetric_fixture(wt);
    let grit = open_grit_repo(wt);
    let token = format!("{main_ref}...{side_ref}");

    let mut opts = RevListOptions {
        left_right: true,
        left_right_explicit: true,
        cherry_pick: true,
        ..RevListOptions::default()
    };
    let (pos, neg) = expand_token_for_rev_list(&grit, &token, &mut opts).expect("sym");
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--left-right", "--cherry-pick", &token],
        &pos,
        &neg,
        opts.clone(),
        "cherry-pick",
    );

    let mut left_opts = RevListOptions {
        left_right: true,
        left_only: true,
        ..RevListOptions::default()
    };
    let (pos, neg) = expand_token_for_rev_list(&grit, &token, &mut left_opts).expect("sym");
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--left-right", "--left-only", &token],
        &pos,
        &neg,
        left_opts,
        "left-only",
    );

    let mut right_opts = RevListOptions {
        left_right: true,
        right_only: true,
        ..RevListOptions::default()
    };
    let (pos, neg) = expand_token_for_rev_list(&grit, &token, &mut right_opts).expect("sym");
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--left-right", "--right-only", &token],
        &pos,
        &neg,
        right_opts,
        "right-only",
    );
}

#[test]
fn path_limited_and_maximal_only_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "tracked.txt", "1\n", "init");
    commit_file(wt, "other.txt", "2\n", "second");
    commit_file(wt, "tracked.txt", "3\n", "touch tracked");
    let grit = open_grit_repo(wt);

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["main", "--", "tracked.txt"],
        &[String::from("main")],
        &[],
        RevListOptions {
            paths: vec!["tracked.txt".to_owned()],
            ..RevListOptions::default()
        },
        "path-limited default simplify",
    );

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--full-history", "main", "--", "tracked.txt"],
        &[String::from("main")],
        &[],
        RevListOptions {
            paths: vec!["tracked.txt".to_owned()],
            full_history: true,
            ..RevListOptions::default()
        },
        "path-limited full-history",
    );

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--simplify-merges", "main", "--", "tracked.txt"],
        &[String::from("main")],
        &[],
        RevListOptions {
            paths: vec!["tracked.txt".to_owned()],
            simplify_merges: true,
            ..RevListOptions::default()
        },
        "path-limited simplify-merges",
    );
}

#[test]
fn rev_list_objects_and_filters_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "aaa\n", "a");
    commit_file(wt, "b.txt", "bbb\n", "b");
    let grit = open_grit_repo(wt);

    let git_objects = support::git_rev_list(wt, &["--objects", "HEAD"]);
    let grit_objects = grit_lib::revwalk::rev_list::rev_list(
        &grit,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            objects: true,
            ..RevListOptions::default()
        },
    )
    .expect("grit objects walk");
    let mut grit_set: std::collections::BTreeSet<ObjectId> =
        grit_objects.commits.into_iter().collect();
    for (oid, _) in grit_objects.objects {
        grit_set.insert(oid);
    }
    let git_set: std::collections::BTreeSet<ObjectId> = git_objects.into_iter().collect();
    assert_eq!(grit_set, git_set);

    let git_blob_none = support::git_rev_list(wt, &["--objects", "--filter=blob:none", "HEAD"]);
    let grit_blob_none = grit_lib::revwalk::rev_list::rev_list(
        &grit,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            objects: true,
            filter: Some(ObjectFilter::BlobNone),
            ..RevListOptions::default()
        },
    )
    .expect("blob none");
    let mut grit_bn: std::collections::BTreeSet<ObjectId> =
        grit_blob_none.commits.into_iter().collect();
    for (oid, _) in grit_blob_none.objects {
        grit_bn.insert(oid);
    }
    assert_eq!(grit_bn, git_blob_none.into_iter().collect());

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--topo-order", "--author-date-order", "HEAD"],
        &[String::from("HEAD")],
        &[],
        RevListOptions {
            ordering: OrderingMode::AuthorDateTopo,
            ..RevListOptions::default()
        },
        "author-date-topo",
    );

    let git_count_objects: usize = git(wt, &["rev-list", "--count", "--objects", "HEAD"])
        .trim()
        .parse()
        .expect("git object count");
    let grit_count = grit_lib::revwalk::rev_list::rev_list(
        &grit,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            count: true,
            objects: true,
            ..RevListOptions::default()
        },
    )
    .expect("grit count objects")
    .count_with_objects_total();
    assert_eq!(grit_count, git_count_objects);
}

#[test]
fn rev_list_option_smoke_exercises_library_paths() {
    let (repo, _base, _mid, _tip) = rich_history_repo();
    let wt = repo.worktree();
    let grit = open_grit_repo(wt);
    let specs = vec![String::from("main")];
    let combos: Vec<RevListOptions> = vec![
        RevListOptions {
            quiet: true,
            count: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            output_mode: grit_lib::revwalk::rev_list::OutputMode::Parents,
            parent_rewrite: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            sparse: true,
            paths: vec!["f.txt".to_owned()],
            ..RevListOptions::default()
        },
        RevListOptions {
            show_pulls: true,
            first_parent: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            exclude_first_parent_only: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            max_parents: Some(1),
            ..RevListOptions::default()
        },
        RevListOptions {
            objects: true,
            filter: Some(ObjectFilter::parse("tree:2").expect("filter")),
            filter_print_omitted: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            objects: true,
            no_object_names: true,
            quiet: true,
            ..RevListOptions::default()
        },
        RevListOptions {
            ordering: OrderingMode::AuthorDateTopo,
            ..RevListOptions::default()
        },
        RevListOptions {
            all_refs: true,
            count: true,
            ..RevListOptions::default()
        },
    ];
    for opts in combos {
        let _ = grit_lib::revwalk::rev_list::rev_list(&grit, &specs, &[], &opts)
            .expect("rev_list smoke");
    }
    let _ = grit_lib::revwalk::rev_list::select_bisect_commits(
        &grit,
        &grit_rev_list_commits_in_repo(&grit, &specs, &[], &RevListOptions::default())
            .expect("tips"),
        false,
    )
    .expect("bisect");
}

const ISSUE_1067_DATE: &str = "2026-01-01T00:00:00Z";

fn git_with_commit_dates(worktree: &Path, date: &str, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(args)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

// Issue #1067: equal committer dates on a merge DAG with bounded `--max-count`.
fn issue_1067_equal_date_merge_dag(worktree: &Path) {
    git_with_commit_dates(
        worktree,
        ISSUE_1067_DATE,
        &[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@b.c",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    git_with_commit_dates(worktree, ISSUE_1067_DATE, &["switch", "-c", "side"]);
    git_with_commit_dates(
        worktree,
        ISSUE_1067_DATE,
        &["commit", "--allow-empty", "-m", "side"],
    );
    git_with_commit_dates(worktree, ISSUE_1067_DATE, &["switch", "main"]);
    git_with_commit_dates(
        worktree,
        ISSUE_1067_DATE,
        &["commit", "--allow-empty", "-m", "mainedit"],
    );
    git_with_commit_dates(
        worktree,
        ISSUE_1067_DATE,
        &["merge", "--no-ff", "side", "-m", "merge"],
    );
}

fn assert_no_ancestor_before_descendant(worktree: &Path, order: &[ObjectId]) {
    for (i, &ancestor) in order.iter().enumerate() {
        for &maybe_descendant in &order[i + 1..] {
            if git_merge_base_is_ancestor(worktree, &ancestor.to_hex(), &maybe_descendant.to_hex())
            {
                panic!(
                    "commit {ancestor} is an ancestor of {maybe_descendant} but appears earlier in the list"
                );
            }
        }
    }
}

#[test]
fn issue_1067_bounded_rev_list_matches_git_on_equal_dates() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    issue_1067_equal_date_merge_dag(wt);
    let grit = open_grit_repo(wt);

    let git_subjects: Vec<String> = git(wt, &["log", "--format=%s"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        git_subjects,
        vec!["merge", "mainedit", "side", "base"],
        "git fixture order"
    );

    for max in [None, Some(10usize), Some(3)] {
        let mut git_args = vec!["HEAD".to_owned()];
        if let Some(n) = max {
            git_args.insert(0, n.to_string());
            git_args.insert(0, "--max-count".to_owned());
        }
        let git_arg_refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let opts = RevListOptions {
            max_count: max,
            ..RevListOptions::default()
        };
        assert_rev_list_commits_match_git(
            wt,
            &grit,
            &git_arg_refs,
            &[String::from("HEAD")],
            &[],
            opts.clone(),
            "issue 1067 max-count",
        );
        let grit_oids = grit_rev_list_commits_in_repo(&grit, &[String::from("HEAD")], &[], &opts)
            .expect("grit rev_list");
        assert_no_ancestor_before_descendant(wt, &grit_oids);
    }
}

#[test]
fn exclude_first_parent_only_with_range_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(
        wt,
        &[
            ("b", "a"),
            ("c", "b"),
            ("side", "a"),
            ("m", "c"),
            ("m", "side"),
            ("tip", "m"),
        ],
        &[("tip", "refs/heads/main"), ("side", "refs/heads/side")],
    );
    let grit = open_grit_repo(wt);
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[
            "--exclude-first-parent-only",
            "--max-count=1",
            "side",
            "^main",
        ],
        &[String::from("side")],
        &[String::from("main")],
        RevListOptions {
            max_count: Some(1),
            exclude_first_parent_only: true,
            ..RevListOptions::default()
        },
        "exclude-first-parent-only",
    );
}

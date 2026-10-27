//! Regression tests for bounded `rev-list` / `grit log` fast paths.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use grit_lib::revwalk::RevWalkListOptions;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(odb: &Odb) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    odb.write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(
    odb: &Odb,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
    author_time: i64,
    committer_time: i64,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let tree = empty_tree(odb)?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {author_time} +0000\ncommitter T <t@e.com> {committer_time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    Repository::open(dir, &RepositoryOptions::empty())
}

#[test]
fn deep_range_count_with_git_and_grit_commit_graphs_matches_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let mut tip = write_commit(repo.odb(), &[], "root", 100, 100).expect("root");
    for i in 1..1024 {
        tip = write_commit(repo.odb(), &[tip], "chain", 100 + i, 100 + i).expect("commit");
    }
    support::git(
        dir.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );

    for grit_graph in [false, true] {
        if grit_graph {
            grit_lib::maintenance::write_commit_graph(
                &repo,
                &grit_lib::maintenance::CommitGraphOptions::default(),
            )
            .expect("grit commit-graph");
        } else {
            support::git(dir.path(), &["commit-graph", "write", "--reachable"]);
        }
        support::git(dir.path(), &["commit-graph", "verify"]);
        support::git(dir.path(), &["fsck", "--strict"]);
        // Open a fresh handle for each writer so a cached graph cannot mask an incompatibility.
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
        for (range, expected) in [
            ("HEAD~20..HEAD", 20),
            ("HEAD..HEAD~20", 0),
            ("HEAD..HEAD", 0),
            ("HEAD~40..HEAD~20", 20),
        ] {
            let from_git: usize = support::git(dir.path(), &["rev-list", "--count", range])
                .trim()
                .parse()
                .expect("git count");
            assert_eq!(from_git, expected);
            assert_eq!(
                repo.rev_walk()
                    .count(&[range], &RevWalkListOptions::default())
                    .expect("count"),
                from_git,
                "{range}, grit graph: {grit_graph}"
            );
            let (positive, negative) = grit_lib::revwalk::rev_list::split_revision_token(range);
            let result = rev_list(
                &repo,
                &positive,
                &negative,
                &RevListOptions {
                    count: true,
                    use_commit_graph: true,
                    ..Default::default()
                },
            )
            .expect("rev-list count");
            assert_eq!(result.commit_count(), from_git);
        }
        assert_eq!(
            repo.rev_walk()
                .count(&["HEAD"], &RevWalkListOptions::default())
                .expect("full count"),
            1024
        );
    }
}

#[test]
fn graph_range_counts_across_merges_and_skewed_dates_match_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 100, 100).expect("root");
    let a = write_commit(repo.odb(), &[root], "a", 300, 300).expect("a");
    let b = write_commit(repo.odb(), &[root], "b", 200, 200).expect("b");
    let merge = write_commit(repo.odb(), &[a, b], "merge", 50, 50).expect("merge");
    let tip = write_commit(repo.odb(), &[merge], "tip", 40, 40).expect("tip");
    support::git(
        dir.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );
    support::git(dir.path(), &["commit-graph", "write", "--reachable"]);
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    for include in [vec![tip], vec![a], vec![a, b], vec![root]] {
        for exclude in [vec![root], vec![a], vec![a, b], vec![tip]] {
            let positive: Vec<_> = include.iter().map(|oid| oid.to_hex()).collect();
            let negative: Vec<_> = exclude.iter().map(|oid| oid.to_hex()).collect();
            let mut tokens = positive.clone();
            tokens.extend(negative.iter().map(|s| format!("^{s}")));
            let tokens: Vec<_> = tokens.iter().map(String::as_str).collect();
            let mut git_args = vec!["rev-list", "--count"];
            git_args.extend_from_slice(&tokens);
            let expected: usize = support::git(dir.path(), &git_args)
                .trim()
                .parse()
                .expect("git count");
            assert_eq!(
                repo.rev_walk()
                    .count(&tokens, &RevWalkListOptions::default())
                    .expect("count"),
                expected,
                "{tokens:?}"
            );
            assert_eq!(
                rev_list(
                    &repo,
                    &positive,
                    &negative,
                    &RevListOptions {
                        count: true,
                        use_commit_graph: true,
                        ..Default::default()
                    },
                )
                .expect("rev-list count")
                .commit_count(),
                expected,
                "{tokens:?}"
            );
        }
    }
}

#[test]
fn bounded_log_on_depth_one_shallow_does_not_emit_missing_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).expect("shallow");

    let opts = RevListOptions {
        max_count: Some(11),
        ..Default::default()
    };
    let result = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![tip]);
}

#[test]
fn rev_list_errors_on_missing_parent_without_shallow() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    let opts = RevListOptions {
        max_count: Some(5),
        ..Default::default()
    };
    let err = rev_list(&repo, &[tip.to_hex()], &[], &opts).unwrap_err();
    assert!(matches!(err, Error::ObjectNotFound(_)));
}

#[test]
fn author_date_order_with_max_count_uses_author_timestamps() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 50, 50).expect("root");
    // `a`: older author, newer committer.
    let a = write_commit(repo.odb(), &[root], "a", 100, 400).expect("a");
    // `b`: newer author, older committer.
    let b = write_commit(repo.odb(), &[root], "b", 300, 200).expect("b");

    let opts = RevListOptions {
        max_count: Some(2),
        ordering: OrderingMode::AuthorDateWalk,
        ..Default::default()
    };
    let result = rev_list(&repo, &[a.to_hex(), b.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![b, a]);
}

#[test]
fn exclude_first_parent_only_with_max_count_matches_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let first = write_commit(repo.odb(), &[root], "first", 20, 20).expect("first");
    let second = write_commit(repo.odb(), &[root], "second", 30, 30).expect("second");
    let merge = write_commit(repo.odb(), &[first, second], "merge", 40, 40).expect("merge");

    let opts = RevListOptions {
        max_count: Some(1),
        exclude_first_parent_only: true,
        ..Default::default()
    };
    let result = rev_list(&repo, &[second.to_hex()], &[merge.to_hex()], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![second]);
}

#[test]
fn bounded_path_limit_stops_at_first_matching_commit() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    let touch = support::commit_file(wt, "tracked.txt", "t\n", "touch tracked");
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(1),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![touch]);

    support::assert_grit_commits_match_git(
        wt,
        &["-n", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_skip_and_max_match_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v0\n", "v0");
    let mut prior = support::commit_file(wt, "tracked.txt", "v1\n", "v1");
    for i in 2..80 {
        let content = format!("v{i}\n");
        let msg = format!("v{i}");
        prior = support::commit_file(wt, "tracked.txt", &content, &msg);
    }
    let _newest = prior;

    let opts = RevListOptions {
        max_count: Some(3),
        skip: 1,
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "3", "--skip", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_zero_emits_nothing_at_matching_tip() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let opts = RevListOptions {
        max_count: Some(0),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["--max-count=0", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn large_max_count_without_paths_matches_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(10_000),
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "10000", "HEAD"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert!(!result.commits.is_empty());
}

/// A merge-heavy history with many equal committer dates and some clock skew, large enough
/// that a `--max-count` page crosses the bounded-walk cap. Returns the tip.
fn write_tied_skewed_history(odb: &Odb, commits: usize) -> grit_lib::objects::ObjectId {
    *write_tied_skewed_history_commits(odb, commits)
        .last()
        .expect("at least one commit")
}

/// Like [`write_tied_skewed_history`], returning every commit oldest first.
fn write_tied_skewed_history_commits(
    odb: &Odb,
    commits: usize,
) -> Vec<grit_lib::objects::ObjectId> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut written: Vec<grit_lib::objects::ObjectId> = Vec::with_capacity(commits);
    for i in 0..commits {
        let mut parents = Vec::new();
        if let Some(&prev) = written.last() {
            parents.push(prev);
        }
        if written.len() > 8 && next() % 4 == 0 {
            let back = 2 + (next() as usize % 6);
            parents.push(written[written.len() - back]);
        }
        // Runs of three commits share a timestamp; every so often a commit is older than its
        // parent.
        let mut when = 1_000_000 + (i as i64 / 3) * 10;
        if next() % 13 == 0 {
            when -= 25;
        }
        let commit = write_commit(odb, &parents, &format!("c{i}"), when, when).expect("commit");
        written.push(commit);
    }
    written
}

#[test]
fn unlimited_range_does_not_read_history_below_excluded_tip() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    // Nothing below `base` exists, so walking either side's full closure fails.
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let base = write_commit(repo.odb(), &[missing], "base", 100, 100).expect("base");
    let mut range = Vec::new();
    let mut parent = base;
    for i in 0..20 {
        let when = 200 + i * 10;
        parent = write_commit(repo.odb(), &[parent], &format!("c{i}"), when, when).expect("c");
        range.push(parent);
    }
    range.reverse();
    let tip_hex = range[0].to_hex();
    let base_hex = base.to_hex();

    let listed = rev_list(
        &repo,
        std::slice::from_ref(&tip_hex),
        std::slice::from_ref(&base_hex),
        &RevListOptions::default(),
    )
    .expect("unlimited A..B stays above the excluded tip");
    assert_eq!(listed.commits, range);

    let spec = format!("{base_hex}..{tip_hex}");
    let walked = repo
        .rev_walk()
        .rev_list_result(&[spec.as_str()], &RevWalkListOptions::default())
        .expect("RevWalk::rev_list_result A..B");
    assert_eq!(walked.commits, range);
    assert_eq!(support::git_rev_list(dir.path(), &[&spec]), range);
}

#[test]
fn range_with_newer_hide_tip_tolerates_missing_history_behind_it() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    // The hide side is newer than the range, so date order reaches behind `hide` before the
    // positive side meets it. Git skips unreadable parents of excluded commits.
    let below = write_commit(repo.odb(), &[missing], "below", 190, 190).expect("below");
    let hide = write_commit(repo.odb(), &[below], "hide", 200, 200).expect("hide");
    let mid = write_commit(repo.odb(), &[hide], "mid", 50, 50).expect("mid");
    let tip = write_commit(repo.odb(), &[mid], "tip", 100, 100).expect("tip");
    let spec = format!("{hide}..{tip}");
    assert_eq!(support::git_rev_list(dir.path(), &[&spec]), vec![tip, mid]);

    for max_count in [None, Some(10)] {
        let listed = rev_list(
            &repo,
            &[tip.to_hex()],
            &[hide.to_hex()],
            &RevListOptions {
                max_count,
                ..Default::default()
            },
        )
        .expect("range with a newer hide tip");
        assert_eq!(listed.commits, vec![tip, mid], "max_count {max_count:?}");
    }
}

/// Rewrite the committer date git stored for `oid` in the repository's commit-graph file.
fn rewrite_commit_graph_date(git_dir: &Path, oid: grit_lib::objects::ObjectId, date: u32) {
    let path = git_dir.join("objects/info/commit-graph");
    let mut bytes = fs::read(&path).expect("read commit-graph");
    let chunk_count = bytes[6] as usize;
    let chunk_offset = |id: &[u8; 4]| {
        (0..chunk_count)
            .map(|i| 8 + i * 12)
            .find(|&at| &bytes[at..at + 4] == id)
            .map(|at| {
                let mut offset = [0u8; 8];
                offset.copy_from_slice(&bytes[at + 4..at + 12]);
                u64::from_be_bytes(offset) as usize
            })
            .expect("chunk present")
    };
    let (oid_lookup, commit_data) = (chunk_offset(b"OIDL"), chunk_offset(b"CDAT"));
    let position = (0..)
        .find(|i| &bytes[oid_lookup + i * 20..oid_lookup + (i + 1) * 20] == oid.as_bytes())
        .expect("commit in graph");
    // Each CDAT row ends with generation and date; the low 32 bits hold the date.
    let date_at = commit_data + position * 36 + 32;
    bytes[date_at..date_at + 4].copy_from_slice(&date.to_be_bytes());
    fs::remove_file(&path).expect("remove read-only commit-graph");
    fs::write(&path, bytes).expect("write commit-graph");
}

#[test]
fn range_with_commit_graph_disabled_reads_commit_objects() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let older = write_commit(repo.odb(), &[root], "older", 20, 20).expect("older");
    let newer = write_commit(repo.odb(), &[root], "newer", 30, 30).expect("newer");
    let merge = write_commit(repo.odb(), &[older, newer], "merge", 40, 40).expect("merge");
    support::git(
        dir.path(),
        &["update-ref", "refs/heads/main", &merge.to_hex()],
    );
    support::git(dir.path(), &["commit-graph", "write", "--reachable"]);
    rewrite_commit_graph_date(dir.path(), older, 1_000);

    let walk = |use_commit_graph: bool, max_count: Option<usize>| {
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
        rev_list(
            &repo,
            &[merge.to_hex()],
            &[root.to_hex()],
            &RevListOptions {
                use_commit_graph,
                max_count,
                ..Default::default()
            },
        )
        .expect("rev-list")
        .commits
    };
    assert_eq!(
        walk(true, None),
        vec![merge, older, newer],
        "the rewritten graph date is what the commit-graph walk sees"
    );
    let spec = format!("{root}..{merge}");
    let git_from_objects: Vec<grit_lib::objects::ObjectId> = support::git(
        dir.path(),
        &["-c", "core.commitGraph=false", "rev-list", &spec],
    )
    .lines()
    .map(|line| line.trim().parse().expect("oid"))
    .collect();
    assert_eq!(git_from_objects, vec![merge, newer, older]);
    for max_count in [None, Some(10)] {
        assert_eq!(
            walk(false, max_count),
            git_from_objects,
            "use_commit_graph: false orders by commit object dates (max_count {max_count:?})"
        );
    }
}

#[test]
fn unlimited_ranges_match_git_on_tied_and_skewed_history() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = write_tied_skewed_history_commits(repo.odb(), 900);
    let hex = |i: usize| commits[i].to_hex();
    let tip = commits.len() - 1;
    let cases: [(&[usize], &[usize]); 5] = [
        (&[tip], &[tip - 20]),
        (&[tip], &[tip - 400]),
        (&[tip], &[0]),
        (&[tip, tip - 300], &[tip - 150, tip - 500]),
        (&[tip - 10], &[tip]),
    ];
    for (positive, negative) in cases {
        let positive: Vec<String> = positive.iter().map(|&i| hex(i)).collect();
        let negative: Vec<String> = negative.iter().map(|&i| hex(i)).collect();
        let mut git_args: Vec<String> = positive.clone();
        git_args.extend(negative.iter().map(|n| format!("^{n}")));
        for reverse in [false, true] {
            let mut args: Vec<&str> = git_args.iter().map(String::as_str).collect();
            if reverse {
                args.push("--reverse");
            }
            let expected = support::git_rev_list(dir.path(), &args);
            let opts = RevListOptions {
                reverse,
                path_graph_reorder: false,
                use_commit_graph: true,
                ..Default::default()
            };
            let actual = rev_list(&repo, &positive, &negative, &opts)
                .expect("rev-list")
                .commits;
            assert_eq!(actual, expected, "rev-list {args:?} differs from git");
        }
    }
}

#[test]
fn date_order_ranges_keep_children_before_parents_like_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = write_tied_skewed_history_commits(repo.odb(), 600);
    let tip = commits[commits.len() - 1].to_hex();
    let hide = commits[100].to_hex();
    for max_count in [Some(40usize), None] {
        let mut args = vec!["--date-order".to_owned(), tip.clone(), format!("^{hide}")];
        if let Some(n) = max_count {
            args.push(format!("--max-count={n}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let expected = support::git_rev_list(dir.path(), &args);
        let opts = RevListOptions {
            ordering: OrderingMode::DateOrderWalk,
            max_count,
            path_graph_reorder: false,
            ..Default::default()
        };
        let actual = rev_list(
            &repo,
            std::slice::from_ref(&tip),
            std::slice::from_ref(&hide),
            &opts,
        )
        .expect("rev-list --date-order")
        .commits;
        assert_eq!(actual, expected, "rev-list {args:?} differs from git");
    }
}

#[test]
fn default_rev_list_options_max_count_matches_git_on_large_history() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 4_300);
    let tip_hex = tip.to_hex();
    let opts = RevListOptions {
        max_count: Some(10),
        ..Default::default()
    };
    let expected = support::git_rev_list(dir.path(), &["-n", "10", &tip_hex]);
    let actual = rev_list(&repo, std::slice::from_ref(&tip_hex), &[], &opts)
        .expect("rev-list")
        .commits;
    assert_eq!(
        actual, expected,
        "plain Default options should use bounded -n"
    );

    let walked = repo
        .rev_walk()
        .walk(&tip_hex, &opts)
        .expect("RevWalk::walk")
        .commits;
    assert_eq!(walked, expected, "RevWalk::walk should match rev_list");
}

#[test]
fn large_max_count_matches_git_order_with_tied_and_skewed_dates() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 4_300);
    let tip_hex = tip.to_hex();

    for (max_count, skip) in [(4_200usize, 0usize), (10, 0), (4_100, 50), (10_000, 0)] {
        // Same shape as the options `RevWalk` builds for `grit log` / `grit revs`.
        let opts = RevListOptions {
            max_count: Some(max_count),
            skip,
            path_graph_reorder: false,
            use_commit_graph: true,
            ..Default::default()
        };
        let max_arg = max_count.to_string();
        let skip_arg = skip.to_string();
        let expected = support::git_rev_list(
            dir.path(),
            &["--max-count", &max_arg, "--skip", &skip_arg, &tip_hex],
        );
        let actual = rev_list(&repo, std::slice::from_ref(&tip_hex), &[], &opts)
            .expect("rev-list")
            .commits;
        assert_eq!(
            actual, expected,
            "rev-list --max-count {max_count} --skip {skip} order differs from git"
        );

        let reversed = rev_list(
            &repo,
            std::slice::from_ref(&tip_hex),
            &[],
            &RevListOptions {
                reverse: true,
                ..opts
            },
        )
        .expect("rev-list --reverse")
        .commits;
        let mut expected_reversed = expected;
        expected_reversed.reverse();
        assert_eq!(reversed, expected_reversed);
    }
}

#[test]
fn retain_commit_data_returns_parsed_listed_commits() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 60);
    let tip_hex = tip.to_hex();
    let base = RevListOptions {
        max_count: Some(25),
        skip: 5,
        path_graph_reorder: false,
        ..Default::default()
    };

    let plain = rev_list(&repo, std::slice::from_ref(&tip_hex), &[], &base).expect("rev-list");
    assert!(plain.commit_data.is_empty(), "nothing is kept unless asked");

    let retained = rev_list(
        &repo,
        std::slice::from_ref(&tip_hex),
        &[],
        &RevListOptions {
            retain_commit_data: true,
            ..base
        },
    )
    .expect("rev-list retaining commits");
    assert_eq!(retained.commits, plain.commits);
    assert_eq!(retained.commits.len(), 25);
    assert_eq!(retained.commit_data.len(), retained.commits.len());
    for oid in &retained.commits {
        let expected = parse_commit(&repo.odb().read(oid).expect("read").data).expect("parse");
        assert_eq!(retained.commit_data.get(oid), Some(&expected), "{oid}");
    }
}

#[test]
fn merge_tip_limit_two_uses_date_order_not_first_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let first = write_commit(repo.odb(), &[root], "first", 20, 20).expect("first");
    let second = write_commit(repo.odb(), &[root], "second", 30, 30).expect("second");
    let merge = write_commit(repo.odb(), &[first, second], "merge", 40, 40).expect("merge");
    let merge_hex = merge.to_hex();
    let expected = support::git_rev_list(dir.path(), &["-n", "2", &merge_hex]);
    let result = repo
        .rev_walk()
        .rev_list_result(
            &[merge_hex.as_str()],
            &RevWalkListOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits, expected);
    assert_eq!(result.commits.len(), 2);
    assert_eq!(
        result.commits[1], second,
        "newer second parent should precede first parent in default order"
    );
}

#[test]
fn non_commit_tip_skips_fast_path_and_matches_rev_list() {
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    support::git(
        dir.path(),
        &["tag", "-a", "tree-tag", "-m", "tree", EMPTY_TREE],
    );
    let list_opts = RevListOptions {
        max_count: Some(1),
        ..Default::default()
    };
    let general = rev_list(&repo, &["tree-tag".to_owned()], &[], &list_opts);
    let walk = repo.rev_walk().rev_list_result(
        &["tree-tag"],
        &RevWalkListOptions {
            limit: Some(1),
            ..Default::default()
        },
    );
    match (&general, &walk) {
        (Ok(g), Ok(w)) => assert_eq!(g.commits, w.commits),
        (Err(_), Err(_)) => {}
        _ => panic!("fast path changed outcome: general={general:?} walk={walk:?}"),
    }
}

#[test]
fn rev_walk_single_positive_limit_one_matches_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    for i in 0..50 {
        support::commit_file(wt, &format!("f{i}.txt"), "x\n", &format!("m{i}"));
    }
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let expected = support::git_rev_list(wt, &["-n", "1", "HEAD~20"]);
    let result = grit_repo
        .rev_walk()
        .rev_list_result(
            &["HEAD~20"],
            &RevWalkListOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits, expected);
}

#[test]
fn rev_walk_forwards_retain_commit_data() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 30);
    let tip_hex = tip.to_hex();
    let result = repo
        .rev_walk()
        .rev_list_result(
            &[tip_hex.as_str()],
            &RevWalkListOptions {
                limit: Some(10),
                retain_commit_data: true,
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits.len(), 10);
    assert!(result
        .commits
        .iter()
        .all(|oid| result.commit_data.contains_key(oid)));
}

/// A merge-heavy history packed by git, so a bounded walk parses a frontier beyond the page.
fn packed_merge_history() -> support::HistoryRepo {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "base.txt", "base\n", "base");
    for i in 0..8 {
        support::git(wt, &["checkout", "-q", "-b", &format!("side{i}")]);
        support::commit_file(wt, &format!("side{i}.txt"), "s\n", &format!("side {i}"));
        support::git(wt, &["checkout", "-q", "main"]);
        support::commit_file(wt, &format!("main{i}.txt"), "m\n", &format!("main {i}"));
        support::git(
            wt,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {i}"),
                &format!("side{i}"),
            ],
        );
    }
    support::git(wt, &["repack", "-a", "-d", "-q"]);
    support::git(wt, &["prune-packed"]);
    repo
}

#[test]
fn retained_page_from_git_pack_matches_git_order_and_commit_bodies() {
    let repo = packed_merge_history();
    let wt = repo.worktree();
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    for limit in [1usize, 2, 5, 17, usize::MAX] {
        let expected = if limit == usize::MAX {
            support::git_rev_list(wt, &["HEAD"])
        } else {
            support::git_rev_list(wt, &["-n", &limit.to_string(), "HEAD"])
        };
        let result = grit_repo
            .rev_walk()
            .rev_list_result(
                &["HEAD"],
                &RevWalkListOptions {
                    limit: Some(limit),
                    retain_commit_data: true,
                    ..Default::default()
                },
            )
            .expect("rev walk");
        assert_eq!(result.commits, expected, "limit {limit}");
        assert_eq!(result.commit_data.len(), expected.len(), "limit {limit}");
        for oid in &result.commits {
            let body = support::git(wt, &["cat-file", "commit", &oid.to_hex()]);
            let from_git = parse_commit(body.as_bytes()).expect("parse git body");
            assert_eq!(result.commit_data.get(oid), Some(&from_git), "{oid}");
        }
    }
}

#[test]
fn bounded_path_limit_extreme_max_count_does_not_panic() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(usize::MAX),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits.len(), 1);
}

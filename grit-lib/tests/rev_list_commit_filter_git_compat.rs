//! `rev-list` commit filters vs system `git rev-list` (author, committer, grep, mailmap).

mod support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::mailmap::load_mailmap_table;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::filter::{CommitFilter, CommitFilterBuilder};
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use support::{
    assert_rev_list_commits_match_git, commit_file, git, git_commit_graph_write, git_rev_list,
    open_grit_repo, HistoryRepo,
};

fn commit_tree_with_identities(
    worktree: &Path,
    label: &str,
    parents: &[ObjectId],
    author: (&str, &str),
    committer: (&str, &str),
    message: &str,
) -> ObjectId {
    let tree = git(worktree, &["mktree"]).trim().to_string();
    let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), message.into()];
    for p in parents {
        args.push("-p".into());
        args.push(p.to_hex());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = Command::new("git")
        .current_dir(worktree)
        .args(&arg_refs)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", committer.0)
        .env("GIT_COMMITTER_EMAIL", committer.1)
        .env("GIT_AUTHOR_DATE", "@1000 +0000")
        .env("GIT_COMMITTER_DATE", "@1000 +0000")
        .env("GIT_CONFIG_GLOBAL", support::null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", support::null_device_for_tests())
        .output()
        .expect("commit-tree");
    assert!(
        out.status.success(),
        "commit-tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let refname = format!("refs/history/{label}");
    git(worktree, &["update-ref", &refname, &commit]);
    commit.parse().expect("oid")
}

struct ParsedRevListCli {
    filter: Option<Arc<CommitFilter>>,
    max_count: Option<usize>,
    min_parents: Option<usize>,
    max_parents: Option<usize>,
    count: bool,
    rev_args: Vec<String>,
    paths: Vec<String>,
}

fn parse_rev_list_cli(args: &[&str]) -> ParsedRevListCli {
    let mut builder = CommitFilterBuilder::new();
    let mut max_count = None;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut count = false;
    let mut rev_args = Vec::new();
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            paths.extend(args[i + 1..].iter().map(|s| (*s).to_owned()));
            break;
        }
        if arg == "--count" {
            count = true;
            i += 1;
            continue;
        }
        if arg == "-n" || arg == "--max-count" {
            max_count = Some(args[i + 1].parse().expect("max-count"));
            i += 2;
            continue;
        }
        if let Some(n) = arg.strip_prefix("-n") {
            max_count = Some(n.parse().expect("max-count"));
            i += 1;
            continue;
        }
        if let Some(n) = arg.strip_prefix("--max-count=") {
            max_count = Some(n.parse().expect("max-count"));
            i += 1;
            continue;
        }
        if arg == "-i" || arg == "--regexp-ignore-case" {
            builder = builder.ignore_case(true);
            i += 1;
            continue;
        }
        if arg == "--all-match" {
            builder = builder.all_match(true);
            i += 1;
            continue;
        }
        if arg == "--invert-grep" {
            builder = builder.invert_grep(true);
            i += 1;
            continue;
        }
        if arg == "-F" || arg == "--fixed-strings" {
            builder = builder.fixed_strings(true);
            i += 1;
            continue;
        }
        if arg == "--author" {
            builder = builder.author(args[i + 1]);
            i += 2;
            continue;
        }
        if let Some(p) = arg.strip_prefix("--author=") {
            builder = builder.author(p);
            i += 1;
            continue;
        }
        if arg == "--committer" {
            builder = builder.committer(args[i + 1]);
            i += 2;
            continue;
        }
        if let Some(p) = arg.strip_prefix("--committer=") {
            builder = builder.committer(p);
            i += 1;
            continue;
        }
        if arg == "--grep" {
            builder = builder.grep(args[i + 1]);
            i += 2;
            continue;
        }
        if let Some(p) = arg.strip_prefix("--grep=") {
            builder = builder.grep(p);
            i += 1;
            continue;
        }
        if arg == "--min-parents" {
            min_parents = Some(args[i + 1].parse().expect("min-parents"));
            i += 2;
            continue;
        }
        if let Some(n) = arg.strip_prefix("--min-parents=") {
            min_parents = Some(n.parse().expect("min-parents"));
            i += 1;
            continue;
        }
        if arg == "--max-parents" {
            max_parents = Some(args[i + 1].parse().expect("max-parents"));
            i += 2;
            continue;
        }
        if let Some(n) = arg.strip_prefix("--max-parents=") {
            max_parents = Some(n.parse().expect("max-parents"));
            i += 1;
            continue;
        }
        rev_args.push(arg.to_owned());
        i += 1;
    }
    let built = builder.build().expect("filter build");
    let filter = built.is_active().then(|| Arc::new(built));
    ParsedRevListCli {
        filter,
        max_count,
        min_parents,
        max_parents,
        count,
        rev_args,
        paths,
    }
}

fn git_rev_list_count(worktree: &Path, args: &[&str]) -> usize {
    let mut argv = vec!["rev-list", "--count"];
    argv.extend(args);
    git(worktree, &argv).trim().parse().expect("count")
}

fn assert_rev_list_filter_matches_git(worktree: &Path, repo: &Repository, args: &[&str]) {
    let git_oids = git_rev_list(worktree, args);
    let parsed = parse_rev_list_cli(args);
    let (positive, negative) = support::expand_revision_tokens(
        &parsed
            .rev_args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    let paths = repo
        .resolve_rev_walk_pathspecs(&parsed.paths)
        .expect("paths");
    let mut opts = RevListOptions {
        max_count: parsed.max_count,
        min_parents: parsed.min_parents,
        max_parents: parsed.max_parents,
        count: parsed.count,
        commit_filter: parsed.filter,
        paths,
        paths_resolved_from_argv: !parsed.paths.is_empty(),
        use_commit_graph: true,
        use_commit_graph_bloom: !parsed.paths.is_empty(),
        ..RevListOptions::default()
    };
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(worktree);
        }
        opts.use_commit_graph = use_graph;
        let result = rev_list(repo, &positive, &negative, &opts).expect("rev_list");
        if parsed.count {
            let git_n = git_rev_list_count(worktree, args);
            let grit_n = result.commit_count();
            assert_eq!(grit_n, git_n, "count mismatch (graph={use_graph})");
        } else {
            assert_eq!(
                result.commits, git_oids,
                "order mismatch (graph={use_graph})"
            );
        }
    }
}

#[test]
fn author_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let a = commit_tree_with_identities(
        wt,
        "a",
        &[],
        ("Alice", "alice@example.com"),
        ("Alice", "alice@example.com"),
        "one",
    );
    let _b = commit_tree_with_identities(
        wt,
        "b",
        &[a],
        ("Bob", "bob@example.com"),
        ("Bob", "bob@example.com"),
        "two",
    );
    git(wt, &["update-ref", "refs/heads/main", &a.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--author=Alice", "main"]);
    assert_rev_list_filter_matches_git(wt, &repo, &["--author", "bob@", "main"]);
}

#[test]
fn committer_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let root = commit_tree_with_identities(wt, "r", &[], ("A", "a@x.com"), ("A", "a@x.com"), "m");
    let tip = commit_tree_with_identities(
        wt,
        "t",
        &[root],
        ("A", "a@x.com"),
        ("Committer", "c@x.com"),
        "m2",
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--committer=Committer", "main"]);
}

#[test]
fn grep_or_and_all_match_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let a = commit_tree_with_identities(
        wt,
        "a",
        &[],
        ("A", "a@x.com"),
        ("A", "a@x.com"),
        "fix parser",
    );
    let b = commit_tree_with_identities(
        wt,
        "b",
        &[a],
        ("A", "a@x.com"),
        ("A", "a@x.com"),
        "fix only",
    );
    git(wt, &["update-ref", "refs/heads/main", &b.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--grep=fix", "main"]);
    assert_rev_list_filter_matches_git(
        wt,
        &repo,
        &["--grep=fix", "--grep=parser", "--all-match", "main"],
    );
}

#[test]
fn invert_grep_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let tip = commit_tree_with_identities(
        wt,
        "t",
        &[],
        ("A", "a@x.com"),
        ("A", "a@x.com"),
        "feature work",
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--grep=fix", "--invert-grep", "main"]);
}

#[test]
fn ignore_case_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let tip = commit_tree_with_identities(
        wt,
        "t",
        &[],
        ("Alice", "a@x.com"),
        ("Alice", "a@x.com"),
        "FIX bug",
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["-i", "--grep=fix", "main"]);
}

#[test]
fn bre_metachars_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let a = commit_tree_with_identities(
        wt,
        "a",
        &[],
        ("A", "a@x.com"),
        ("A", "a@x.com"),
        "fix(parser) done",
    );
    let b = commit_tree_with_identities(wt, "b", &[a], ("A", "a@x.com"), ("A", "a@x.com"), "a+b");
    let c =
        commit_tree_with_identities(wt, "c", &[b], ("A", "a@x.com"), ("A", "a@x.com"), "x or y");
    git(wt, &["update-ref", "refs/heads/main", &c.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--grep=fix(parser)", "main"]);
    assert_rev_list_filter_matches_git(wt, &repo, &["--grep=a+b", "main"]);
    assert_rev_list_filter_matches_git(wt, &repo, &["--grep", r"\(x\|y\)", "main"]);
}

#[test]
fn fixed_strings_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let tip = commit_tree_with_identities(
        wt,
        "t",
        &[],
        ("A", "a@x.com"),
        ("A", "a@x.com"),
        "literal a+b text",
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["-F", "--grep=a+b", "main"]);
}

#[test]
fn mailmap_author_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    std::fs::write(
        wt.join(".mailmap"),
        "Proper Name <proper@email.com> <old@email.com>\n",
    )
    .expect("mailmap");
    git(wt, &["config", "log.mailmap", "true"]);
    let tip = commit_tree_with_identities(
        wt,
        "t",
        &[],
        ("Wrong", "old@email.com"),
        ("Wrong", "old@email.com"),
        "msg",
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    let repo = open_grit_repo(wt);
    let table = load_mailmap_table(&repo).expect("mailmap");
    let filter = Arc::new(
        CommitFilterBuilder::new()
            .author("Proper Name")
            .mailmap(table)
            .build()
            .expect("filter"),
    );
    // System `git rev-list` does not consult `.mailmap`; `git log --use-mailmap` does.
    // Grit applies an explicit [`MailmapTable`] on the filter (library callers load it).
    let git_oids: Vec<ObjectId> = git(
        wt,
        &[
            "log",
            "--use-mailmap",
            "--format=%H",
            "--author=Proper Name",
            "main",
        ],
    )
    .lines()
    .filter_map(|line| line.trim().parse().ok())
    .collect();
    for use_graph in [false, true] {
        if use_graph {
            git_commit_graph_write(wt);
        }
        let grit_oids = rev_list(
            &repo,
            &[String::from("main")],
            &[],
            &RevListOptions {
                commit_filter: Some(filter.clone()),
                use_commit_graph: use_graph,
                ..RevListOptions::default()
            },
        )
        .expect("rev_list")
        .commits;
        assert_eq!(grit_oids, git_oids, "mailmap (graph={use_graph})");
    }
}

#[test]
fn max_count_counts_filtered_commits_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let mut prev = commit_tree_with_identities(
        wt,
        "c0",
        &[],
        ("Alice", "a@x.com"),
        ("Alice", "a@x.com"),
        "m0",
    );
    for i in 1..8 {
        prev = commit_tree_with_identities(
            wt,
            &format!("c{i}"),
            &[prev],
            if i % 2 == 0 {
                ("Alice", "a@x.com")
            } else {
                ("Bob", "b@x.com")
            },
            if i % 2 == 0 {
                ("Alice", "a@x.com")
            } else {
                ("Bob", "b@x.com")
            },
            &format!("m{i}"),
        );
    }
    git(wt, &["update-ref", "refs/heads/main", &prev.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["-n", "3", "--author=Alice", "main"]);
}

#[test]
fn count_with_filter_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let mut prev = commit_tree_with_identities(
        wt,
        "c0",
        &[],
        ("Alice", "a@x.com"),
        ("Alice", "a@x.com"),
        "m",
    );
    for i in 1..5 {
        prev = commit_tree_with_identities(
            wt,
            &format!("c{i}"),
            &[prev],
            ("Alice", "a@x.com"),
            ("Alice", "a@x.com"),
            "m",
        );
    }
    git(wt, &["update-ref", "refs/heads/main", &prev.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--count", "--author=Alice", "main"]);
}

#[test]
fn filter_with_paths_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let root = commit_tree_with_identities(
        wt,
        "root",
        &[],
        ("Bob", "b@x.com"),
        ("Bob", "b@x.com"),
        "root",
    );
    git(wt, &["update-ref", "refs/heads/main", &root.to_hex()]);
    commit_file(wt, "path.txt", "v1", "touch path");
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(
        wt,
        &repo,
        &["--author=Refs Harness", "main", "--", "path.txt"],
    );
}

#[test]
fn filter_through_dropped_merges_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let base = commit_tree_with_identities(
        wt,
        "base",
        &[],
        ("Alice", "a@x.com"),
        ("Alice", "a@x.com"),
        "base",
    );
    let side = commit_tree_with_identities(
        wt,
        "side",
        &[base],
        ("Bob", "b@x.com"),
        ("Bob", "b@x.com"),
        "side",
    );
    let merge = {
        let tree = git(wt, &["mktree"]).trim().to_string();
        let merge_oid = git(
            wt,
            &[
                "commit-tree",
                &tree,
                "-p",
                &base.to_hex(),
                "-p",
                &side.to_hex(),
                "-m",
                "merge",
            ],
        )
        .trim()
        .to_string();
        merge_oid.parse::<ObjectId>().expect("merge")
    };
    git(wt, &["update-ref", "refs/heads/main", &merge.to_hex()]);
    let repo = open_grit_repo(wt);
    assert_rev_list_filter_matches_git(wt, &repo, &["--author=Alice", "--max-parents=1", "main"]);
}

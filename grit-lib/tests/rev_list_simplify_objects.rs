//! Rev-list history simplification, object walks, and filters (upstream t60xx / t4216).

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{
    collect_revision_specs_from_stdin_lines, rev_list, MissingAction, ObjectFilter, RevListOptions,
};
use grit_lib::revwalk::rev_list_error::RevListError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use support::{
    assert_grit_commits_match_git, git, git_rev_list, git_rev_list_object_set,
    git_rev_list_with_boundary, grit_rev_list_object_set, ordering_from_git_args,
    setup_dual_merge_back_simplify_oracle, setup_first_parent_boundary_history,
    setup_path_simplify_history, setup_treesame_history, HistoryRepo,
};

fn rev_list_opts_from_git_flags(flags: &[&str]) -> (RevListOptions, Vec<String>, Vec<String>) {
    let mut paths = Vec::new();
    let mut pos = Vec::new();
    let mut neg = Vec::new();
    let mut after_dash = false;
    for arg in flags {
        if after_dash {
            paths.push((*arg).to_owned());
            continue;
        }
        if *arg == "--" {
            after_dash = true;
            continue;
        }
        if arg.contains("..") && !arg.starts_with('-') {
            let parts: Vec<&str> = arg.split("..").collect();
            if parts.len() == 2 {
                pos.push(parts[1].to_owned());
                neg.push(parts[0].to_owned());
                continue;
            }
        }
        if arg.starts_with('^') {
            neg.push(arg.trim_start_matches('^').to_owned());
            continue;
        }
    }
    let opts = RevListOptions {
        full_history: flags.iter().any(|a| *a == "--full-history"),
        simplify_merges: flags.iter().any(|a| *a == "--simplify-merges"),
        ancestry_path: flags.iter().any(|a| *a == "--ancestry-path"),
        first_parent: flags.iter().any(|a| *a == "--first-parent"),
        sparse: flags.iter().any(|a| *a == "--sparse"),
        show_pulls: flags.iter().any(|a| *a == "--show-pulls"),
        ordering: ordering_from_git_args(flags),
        paths,
        ..RevListOptions::default()
    };
    (opts, pos, neg)
}

fn assert_grit_matches_git_rev_list(wt: &Path, git_log_flags: &[&str], extra_pos: Option<&str>) {
    let mut git_args: Vec<&str> = Vec::new();
    if let Some(tip) = extra_pos {
        git_args.push(tip);
    } else if !git_log_flags.iter().any(|a| a.contains("..")) {
        git_args.push("M");
    }
    git_args.extend(git_log_flags);

    let (opts, mut pos, neg) = rev_list_opts_from_git_flags(git_log_flags);
    if let Some(tip) = extra_pos {
        pos.push(tip.to_owned());
    } else if pos.is_empty() {
        pos.push("M".to_owned());
    }
    assert_grit_commits_match_git(wt, &git_args, &pos, &neg, &opts);
}

#[test]
fn t6012_simplification_modes_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_path_simplify_history(wt);

    assert_grit_matches_git_rev_list(wt, &["--", "content.txt"], Some("rw6012-l"));
    assert_grit_matches_git_rev_list(
        wt,
        &["--full-history", "--", "content.txt"],
        Some("rw6012-l"),
    );
    assert_grit_matches_git_rev_list(
        wt,
        &["--simplify-merges", "--", "content.txt"],
        Some("rw6012-l"),
    );
    assert_grit_matches_git_rev_list(wt, &["--first-parent"], Some("rw6012-l"));
}

#[test]
fn simplify_merges_two_positive_tips_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (a0_tip, b0_tip) = setup_dual_merge_back_simplify_oracle(wt);
    let git_args = [
        a0_tip.as_str(),
        b0_tip.as_str(),
        "--simplify-merges",
        "--",
        "content.txt",
    ];
    let (opts, _pos, neg) =
        rev_list_opts_from_git_flags(&["--simplify-merges", "--", "content.txt"]);
    let pos = vec![a0_tip.clone(), b0_tip.clone()];
    assert_grit_commits_match_git(wt, &git_args, &pos, &neg, &opts);
}

#[test]
fn t6111_treesame_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (_f, _m) = setup_treesame_history(wt);

    assert_grit_matches_git_rev_list(wt, &["--", "content.txt"], Some("rw6111-m"));
    assert_grit_matches_git_rev_list(
        wt,
        &["--full-history", "--", "content.txt"],
        Some("rw6111-m"),
    );
    let git_range = git_rev_list(
        wt,
        &["rw6111-f..rw6111-m", "--ancestry-path", "--", "content.txt"],
    );
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        ancestry_path: true,
        paths: vec!["content.txt".to_owned()],
        ..RevListOptions::default()
    };
    let grit_range = rev_list(
        &grit_repo,
        &[String::from("rw6111-m")],
        &[String::from("rw6111-f")],
        &opts,
    )
    .expect("ancestry")
    .commits;
    assert_eq!(grit_range, git_range, "ancestry-path range");
}

#[test]
fn treesame_show_pulls_exercises_simplify_merge_paths() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_treesame_history(wt);

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    for (full_history, preserve) in [(true, true), (false, true), (true, false)] {
        let out = rev_list(
            &grit_repo,
            &[String::from("rw6111-m")],
            &[],
            &RevListOptions {
                full_history,
                simplify_merges: true,
                show_pulls: true,
                preserve_simplify_merges_graph_merges: preserve,
                sparse: false,
                paths: vec!["content.txt".to_owned()],
                ..RevListOptions::default()
            },
        )
        .expect("show-pulls walk");
        assert!(!out.commits.is_empty());
    }
}

#[test]
fn t4216_bloom_and_no_bloom_identical() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "A/file1", "1\n", "c1");
    commit_file(wt, "A/B/file2", "2\n", "c2");
    commit_file(wt, "A/B/C/file3", "3\n", "c3");
    git(
        wt,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );

    let git_oids = git_rev_list(wt, &["HEAD", "--", "A/file1"]);
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    for use_bloom in [false, true] {
        let opts = RevListOptions {
            paths: vec!["A/file1".to_owned()],
            use_commit_graph: true,
            use_commit_graph_bloom: use_bloom,
            ..RevListOptions::default()
        };
        let out = rev_list(&grit_repo, &["HEAD".to_owned()], &[], &opts).expect("rev-list");
        assert_eq!(out.commits, git_oids, "bloom={use_bloom}");
    }
}

fn commit_file(wt: &Path, path: &str, content: &str, msg: &str) {
    support::commit_file(wt, path, content, msg);
}

#[test]
fn t6018_ref_globs_and_exclude_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "foo", "main\n", "main");
    git(wt, &["checkout", "-b", "subspace/one", "main"]);
    commit_file(wt, "foo", "one\n", "one");
    git(wt, &["checkout", "-b", "subspace/two", "main"]);
    commit_file(wt, "foo", "two\n", "two");
    git(wt, &["checkout", "main"]);

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let (pos, _neg, _all, _paths) =
        collect_revision_specs_from_stdin_lines(&grit_repo, &[], &["--glob=refs/heads/subspace/*"])
            .expect("stdin glob");
    let git_oids = git_rev_list(wt, &["--glob=refs/heads/subspace/*"]);
    let grit_oids = rev_list(
        &grit_repo,
        &pos.iter().map(|s| s.clone()).collect::<Vec<_>>(),
        &[],
        &RevListOptions::default(),
    )
    .expect("rev-list")
    .commits;
    assert_eq!(grit_oids.len(), git_oids.len());
    assert_eq!(
        grit_oids.iter().collect::<HashSet<_>>(),
        git_oids.iter().collect::<HashSet<_>>()
    );
}

#[test]
fn t6112_object_filters_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    for n in 1..=5 {
        commit_file(
            wt,
            &format!("file.{n}"),
            &format!("This is file: {n}\n"),
            &n.to_string(),
        );
    }
    let cases: &[(&str, ObjectFilter)] = &[
        ("blob:none", ObjectFilter::BlobNone),
        ("blob:limit=500", ObjectFilter::BlobLimit(500)),
        ("tree:0", ObjectFilter::TreeDepth(0)),
        (
            "object:type=commit",
            ObjectFilter::ObjectType(grit_lib::revwalk::rev_list::FilterObjectKind::Commit),
        ),
    ];
    for (spec, filter) in cases {
        let git_set = git_rev_list_object_set(wt, &["HEAD", &format!("--filter={spec}")]);
        let grit_set =
            grit_rev_list_object_set(wt, &[String::from("HEAD")], &[], Some(filter.clone()));
        assert_eq!(grit_set, git_set, "filter {spec}");
    }
    let combine = ObjectFilter::parse("combine:blob:none+tree:1").expect("combine");
    let git_set = git_rev_list_object_set(wt, &["HEAD", "--filter=combine:blob:none+tree:1"]);
    let grit_set = grit_rev_list_object_set(wt, &[String::from("HEAD")], &[], Some(combine));
    assert_eq!(grit_set, git_set, "combine filter");
}

#[test]
fn t6022_missing_actions_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "1.t", "a\n", "1");
    commit_file(wt, "2.t", "b\n", "2");
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let blob_oid: ObjectId = git(wt, &["rev-parse", "HEAD:1.t"])
        .trim()
        .parse()
        .expect("blob");
    let blob_loose = grit_repo.git_dir().join("objects").join(format!(
        "{}/{}",
        &blob_oid.to_hex()[..2],
        &blob_oid.to_hex()[2..]
    ));
    let blob_backup = blob_loose.with_extension("hidden");
    fs::rename(&blob_loose, &blob_backup).expect("hide blob");
    let _restore_blob = RestoreOnDrop(blob_backup, blob_loose.clone());

    for action in [MissingAction::Allow, MissingAction::Print] {
        let git_args = match action {
            MissingAction::Print => {
                vec!["--missing=print", "--objects", "--no-object-names", "HEAD"]
            }
            _ => vec![
                "--missing=allow-any",
                "--objects",
                "--no-object-names",
                "HEAD",
            ],
        };
        let git_set = git_rev_list_object_set(wt, &git_args);
        let grit_repo =
            Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
        let opts = RevListOptions {
            objects: true,
            no_object_names: true,
            missing_action: action,
            ..RevListOptions::default()
        };
        let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("walk");
        let mut grit_set: HashSet<ObjectId> = result.commits.into_iter().collect();
        grit_set.extend(result.objects.into_iter().map(|(o, _)| o));
        grit_set.extend(result.missing_objects);
        assert_eq!(grit_set, git_set, "missing action {action:?}");
    }
}

struct RestoreOnDrop(PathBuf, PathBuf);

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        let _ = fs::rename(&self.0, &self.1);
    }
}

#[test]
fn t6102_unexpected_object_types_typed_error() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "foo", "foo\n", "first");
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let blob: ObjectId = git(wt, &["rev-parse", "HEAD:foo"])
        .trim()
        .parse()
        .expect("blob");
    let blob_hex = blob.to_hex();
    let mut broken_body = b"40000 foo\0".to_vec();
    broken_body.extend_from_slice(blob.as_bytes());
    let broken_tree = grit_repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &broken_body)
        .expect("broken tree");
    let broken_tree_hex = broken_tree.to_hex();
    let opts = RevListOptions {
        objects: true,
        ..RevListOptions::default()
    };
    let result = rev_list(&grit_repo, &[blob_hex, broken_tree_hex.clone()], &[], &opts);
    match result {
        Err(Error::RevList(RevListError::UnexpectedObjectKind { .. })) => {}
        other => panic!("unexpected result: {other:?} (blob={blob} broken={broken_tree_hex})"),
    }

    let head_tree: ObjectId = git(wt, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("tree");
    let mut broken_blob_body = b"100644 foo\0".to_vec();
    broken_blob_body.extend_from_slice(head_tree.as_bytes());
    let broken_blob_tree = grit_repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &broken_blob_body)
        .expect("broken blob tree");
    let broken_blob_hex = broken_blob_tree.to_hex();
    let git_lone = git_rev_list_object_set(wt, &["--objects", &broken_blob_hex]);
    let grit_lone = rev_list(&grit_repo, &[broken_blob_hex.clone()], &[], &opts)
        .expect("lone malformed tree walk");
    let mut grit_lone_set: HashSet<ObjectId> =
        grit_lone.objects.into_iter().map(|(o, _)| o).collect();
    grit_lone_set.insert(broken_blob_tree);
    assert_eq!(grit_lone_set, git_lone, "lone 100644→tree must match git");

    let tree_hex = git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let err =
        rev_list(&grit_repo, &[tree_hex, broken_blob_hex], &[], &opts).expect_err("non-blob seen");
    assert!(matches!(
        err,
        Error::RevList(RevListError::UnexpectedObjectKind { .. })
    ));
}

#[test]
fn t6017_stdin_revision_specs_match_git_semantics() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a", "1\n", "one");
    commit_file(wt, "b", "2\n", "two");
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let (pos, neg, all, paths) = collect_revision_specs_from_stdin_lines(
        &grit_repo,
        &[String::from("HEAD")],
        &["--not", "HEAD~1"],
    )
    .expect("stdin");
    assert!(!all);
    assert!(paths.is_empty());
    assert_eq!(neg.len(), 1);
    let git_oids = git_rev_list(wt, &["HEAD", "^HEAD~1"]);
    let grit_oids = rev_list(&grit_repo, &pos, &neg, &RevListOptions::default())
        .expect("rev-list")
        .commits;
    assert_eq!(grit_oids, git_oids);
}

#[test]
fn t6004_path_limited_trivial_optimization_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a", "Hello\n", "initial");
    git(wt, &["tag", "initial"]);
    let tree = git(wt, &["rev-parse", "HEAD^{tree}"]);
    let commit = git(
        wt,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            &git(wt, &["rev-parse", "HEAD"]).trim(),
            "-m",
            "unchanged tree",
        ],
    )
    .trim()
    .to_string();
    assert_eq!(git_rev_list(wt, &[&commit]).len(), 2);
    assert_eq!(git_rev_list(wt, &[&commit, "--", "."]).len(), 1);
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        paths: vec![".".to_owned()],
        ..RevListOptions::default()
    };
    let grit = rev_list(&grit_repo, &[commit.clone()], &[], &opts).expect("rev-list");
    assert_eq!(grit.commits.len(), 1);
}

#[test]
fn t6110_first_parent_boundary_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_first_parent_boundary_history(wt);
    let git_lines = git_rev_list_with_boundary(wt, &["--first-parent", "--boundary", "HEAD^.."]);
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        first_parent: true,
        boundary: true,
        ..RevListOptions::default()
    };
    let result = rev_list(
        &grit_repo,
        &[String::from("HEAD")],
        &[String::from("HEAD^")],
        &opts,
    )
    .expect("rev-list");
    let mut grit_lines: Vec<(bool, ObjectId)> =
        result.commits.into_iter().map(|oid| (false, oid)).collect();
    grit_lines.extend(result.boundary_commits.into_iter().map(|oid| (true, oid)));
    assert_eq!(grit_lines, git_lines);
}

#[test]
fn reachable_objects_all_refs_with_filter() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "x\n", "c");
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let walk = |filter: Option<ObjectFilter>| {
        let opts = RevListOptions {
            all_refs: true,
            objects: true,
            no_object_names: true,
            filter,
            ..RevListOptions::default()
        };
        let out = rev_list(&grit_repo, &[], &[], &opts).expect("rev-list");
        out.commits.len() + out.objects.len()
    };
    let all = walk(None);
    assert!(all >= 3);
    assert!(walk(Some(ObjectFilter::BlobNone)) < all);
}

#[test]
fn object_filter_parse_rejects_invalid_specs() {
    assert!(ObjectFilter::parse("not-a-filter").is_err());
    assert!(ObjectFilter::parse("tree:").is_err());
    assert!(ObjectFilter::parse("combine:").is_err());
    assert!(ObjectFilter::parse("object:type=widget").is_err());
    assert_eq!(
        ObjectFilter::parse("blob:none").expect("blob:none"),
        ObjectFilter::BlobNone
    );
}

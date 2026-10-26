//! Object-walk roots: tagged commits, treeish paths, filters, and missing objects vs `git`.

use crate::support;

use std::collections::HashSet;

use grit_lib::revwalk::rev_list::{rev_list, MissingAction, ObjectFilter, RevListOptions};
use support::{commit_file, git, git_rev_list_object_set, grit_rev_list_object_set, HistoryRepo};

#[test]
fn object_walk_treeish_path_and_tagged_commit_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "dir/inner.txt", "inner\n", "add inner");
    commit_file(wt, "top.txt", "top\n", "add top");
    git(
        wt,
        &["tag", "-a", "annotated", "-m", "annotated tag", "HEAD~1"],
    );

    for spec in [
        "HEAD:dir",
        "annotated",
        "HEAD^{tree}",
        "annotated^{}",
        "HEAD^{commit}",
    ] {
        let git_set = git_rev_list_object_set(wt, &[spec]);
        let grit_set = grit_rev_list_object_set(wt, &[spec.to_owned()], &[], None);
        assert_eq!(grit_set, git_set, "object walk {spec}");
    }
}

#[test]
fn object_walk_filters_and_missing_print_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "small.txt", "tiny\n", "small");
    commit_file(wt, "large.bin", &"x".repeat(9000), "large");

    let filters: &[(&str, ObjectFilter)] = &[
        ("blob:none", ObjectFilter::BlobNone),
        ("blob:limit=100", ObjectFilter::BlobLimit(100)),
        ("tree:1", ObjectFilter::TreeDepth(1)),
    ];
    for (spec, filter) in filters {
        let git_set = git_rev_list_object_set(wt, &[&format!("--filter={spec}"), "HEAD"]);
        let grit_set =
            grit_rev_list_object_set(wt, &[String::from("HEAD")], &[], Some(filter.clone()));
        assert_eq!(grit_set, git_set, "filter {spec}");
    }

    let grit_repo = support::open_grit_repo(wt);
    let blob_hex = git(wt, &["rev-parse", "HEAD:small.txt"]).trim().to_string();
    let loose =
        grit_repo
            .git_dir()
            .join("objects")
            .join(format!("{}/{}", &blob_hex[..2], &blob_hex[2..]));
    let backup = loose.with_extension("bak");
    std::fs::rename(&loose, &backup).expect("hide blob");
    let _restore = RestoreOnDrop(backup, loose);

    let git_missing = git(wt, &["rev-list", "--objects", "--missing=print", "HEAD"]);
    let grit_missing = rev_list(
        &grit_repo,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            objects: true,
            missing_action: MissingAction::Print,
            ..RevListOptions::default()
        },
    )
    .expect("missing=print");
    assert!(
        grit_missing
            .missing_objects
            .iter()
            .any(|o| o.to_hex() == blob_hex),
        "grit should report missing blob"
    );
    assert!(
        git_missing.contains(&blob_hex),
        "git should report missing blob: {git_missing}"
    );
}

struct RestoreOnDrop(std::path::PathBuf, std::path::PathBuf);

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::rename(&self.0, &self.1);
    }
}

#[test]
fn object_walk_combine_filter_provided_objects_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "aaa\n", "a");
    commit_file(wt, "b.txt", "bbb\n", "b");
    let combine = ObjectFilter::parse("combine:blob:none+tree:1").expect("combine");
    let git_set = git_rev_list_object_set(wt, &["--filter=combine:blob:none+tree:1", "HEAD"]);
    let grit_repo = support::open_grit_repo(wt);
    let result = rev_list(
        &grit_repo,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            objects: true,
            no_object_names: true,
            filter: Some(combine),
            filter_provided_objects: true,
            ..RevListOptions::default()
        },
    )
    .expect("combine walk");
    let mut grit_set = HashSet::new();
    grit_set.extend(result.commits);
    grit_set.extend(result.objects.iter().map(|(o, _)| *o));
    assert_eq!(grit_set, git_set, "combine filter-provided-objects");
}

#[test]
fn object_walk_annotated_tag_on_blob_peels_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "data.bin", "payload\n", "blob target");
    let blob = git(wt, &["rev-parse", "HEAD:data.bin"]).trim().to_string();
    git(wt, &["tag", "-a", "blob-tag", "-m", "tag on blob", &blob]);
    let git_set = git_rev_list_object_set(wt, &["blob-tag"]);
    let grit_repo = support::open_grit_repo(wt);
    let result = rev_list(
        &grit_repo,
        &[String::from("blob-tag")],
        &[],
        &RevListOptions {
            objects: true,
            ..RevListOptions::default()
        },
    )
    .expect("tagged blob walk");
    let mut grit_set = HashSet::new();
    grit_set.extend(result.commits);
    grit_set.extend(result.objects.iter().map(|(o, _)| *o));
    let blob_oid: grit_lib::objects::ObjectId = blob.parse().expect("blob oid");
    assert!(git_set.contains(&blob_oid));
    assert!(
        grit_set.contains(&blob_oid),
        "grit walk should include tagged blob; grit={grit_set:?} git={git_set:?}"
    );
}

#[test]
fn object_walk_sparse_pathspec_limits_objects_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "dir/inner.txt", "inner\n", "inner");
    commit_file(wt, "other.txt", "other\n", "other");
    let git_set = git_rev_list_object_set(wt, &["HEAD", "--", "dir/"]);
    let grit_repo = support::open_grit_repo(wt);
    let result = rev_list(
        &grit_repo,
        &[String::from("HEAD")],
        &[],
        &RevListOptions {
            objects: true,
            no_object_names: true,
            paths: vec!["dir/".to_owned()],
            ..RevListOptions::default()
        },
    )
    .expect("sparse objects");
    let mut grit_paths = HashSet::new();
    grit_paths.extend(result.commits);
    grit_paths.extend(result.objects.iter().map(|(o, _)| *o));
    assert_eq!(grit_paths, git_set, "path-limited object set");
}

#[test]
fn object_walk_over_merges_and_nested_dir_edits_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a/b/c/deep.txt", "deep\n", "deep");
    commit_file(wt, "a/b/side.txt", "side\n", "side");
    commit_file(wt, "a/top.txt", "top\n", "top");
    git(wt, &["checkout", "-q", "-b", "topic"]);
    commit_file(wt, "a/b/c/deep.txt", "deep on topic\n", "topic deep");
    commit_file(wt, "a/b/new.txt", "new\n", "topic new");
    git(wt, &["checkout", "-q", "-"]);
    commit_file(wt, "a/top.txt", "top on main\n", "main top");
    // Back to an earlier blob, so the object exists in a grandparent but not the parent.
    commit_file(wt, "a/b/side.txt", "side v2\n", "side v2");
    commit_file(wt, "a/b/side.txt", "side\n", "side back");
    git(wt, &["merge", "-q", "--no-edit", "topic"]);
    // `a/b/c` turns from a directory into a file of the same name.
    git(wt, &["rm", "-q", "-r", "a/b/c"]);
    commit_file(wt, "a/b/c", "now a file\n", "dir to file");

    for (positive, negative) in [
        (vec!["HEAD"], vec![]),
        (vec!["HEAD"], vec!["HEAD~3"]),
        (vec!["HEAD"], vec!["topic"]),
        (vec!["topic", "HEAD~1"], vec!["HEAD~5"]),
    ] {
        let mut git_args: Vec<String> = positive.iter().map(|s| s.to_string()).collect();
        git_args.extend(negative.iter().map(|s| format!("^{s}")));
        let git_args: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let git_set = git_rev_list_object_set(wt, &git_args);
        let grit_set = grit_rev_list_object_set(
            wt,
            &positive.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &negative.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            None,
        );
        if negative.is_empty() {
            assert_eq!(grit_set, git_set, "objects for {git_args:?}");
        } else {
            // Excluded ranges may still list extra objects already reachable from the
            // negative side; a pack or bundle built from the walk must never miss one.
            let missing: Vec<_> = git_set.difference(&grit_set).collect();
            assert!(
                missing.is_empty(),
                "objects for {git_args:?} miss {missing:?}"
            );
        }
    }
}

#[test]
fn nested_annotated_tag_tips_peel_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "data.bin", "payload\n", "first");
    commit_file(wt, "more.txt", "more\n", "second");
    git(wt, &["tag", "-a", "inner", "-m", "inner", "HEAD~1"]);
    git(wt, &["tag", "-a", "outer", "-m", "outer", "inner"]);
    let blob = git(wt, &["rev-parse", "HEAD:data.bin"]).trim().to_string();
    git(wt, &["tag", "-a", "blob-inner", "-m", "blob", &blob]);
    git(
        wt,
        &["tag", "-a", "blob-outer", "-m", "blob outer", "blob-inner"],
    );

    let grit_repo = support::open_grit_repo(wt);
    let cases: [(&[&str], &[&str]); 3] = [
        (&["outer"], &[]),
        (&["blob-outer", "outer"], &[]),
        (&["HEAD"], &["outer"]),
    ];
    for (positive, negative) in cases {
        let mut args = vec!["rev-list".to_owned()];
        args.extend(positive.iter().map(|s| (*s).to_owned()));
        args.extend(negative.iter().map(|s| format!("^{s}")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let git_commits: Vec<String> = git(wt, &args).lines().map(str::to_owned).collect();
        let owned = |specs: &[&str]| specs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let grit_commits: Vec<String> = rev_list(
            &grit_repo,
            &owned(positive),
            &owned(negative),
            &RevListOptions::default(),
        )
        .expect("commit walk")
        .commits
        .iter()
        .map(|c| c.to_hex())
        .collect();
        assert_eq!(grit_commits, git_commits, "{args:?}");
    }
}

//! Ref query `--contains`, `--merged`, and `--points-at` parity with `git for-each-ref`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::maintenance::CommitGraphOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::listing::{query_refs, RefNamespace, RefQuery, RefQueryError, RefQueryOptions};
use grit_lib::refs::query_reachability::resolve_commit_filter_target;
use grit_lib::repo::{InitOptions, Repository};
use support::{git, git_interop_available, require_reftable_git, Backend};

fn git_refnames(worktree: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend_from_slice(extra);
    git(worktree, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_refnames(repo: &Repository, query: RefQuery) -> Vec<String> {
    query_refs(repo, &query, &RefQueryOptions::default())
        .expect("query_refs")
        .into_iter()
        .map(|r| r.full_name)
        .collect()
}

fn init_repo(worktree: &std::path::Path, backend: Backend) -> Repository {
    let mut opts = InitOptions::new().with_initial_branch("main".to_owned());
    if backend == Backend::Reftable {
        opts = opts.with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Reftable);
    }
    Repository::init(worktree, &opts).expect("init")
}

fn seed_linear(worktree: &std::path::Path) -> ObjectId {
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "c1"]);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "c2"]);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "c3"]);
    let tip = git(worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    git(worktree, &["tag", "v1"]);
    git(worktree, &["tag", "-a", "v-annot", "-m", "m", "HEAD~1"]);
    ObjectId::from_hex(&tip).expect("tip")
}

fn seed_merge(worktree: &std::path::Path) -> (ObjectId, ObjectId) {
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "root"]);
    git(worktree, &["branch", "side"]);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "side1"]);
    git(worktree, &["checkout", "main"]);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "main1"]);
    git(worktree, &["merge", "--no-edit", "side"]);
    let merge_tip = git(worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    git(worktree, &["branch", "merged-tip", "side"]);
    git(worktree, &["branch", "open-tip", "main~1"]);
    let side_tip = git(worktree, &["rev-parse", "refs/heads/side"])
        .trim()
        .to_owned();
    (
        ObjectId::from_hex(&merge_tip).expect("merge"),
        ObjectId::from_hex(&side_tip).expect("side"),
    )
}

fn seed_tag_of_tag(worktree: &std::path::Path) -> ObjectId {
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "base"]);
    git(worktree, &["tag", "-a", "inner", "-m", "inner", "HEAD"]);
    git(worktree, &["tag", "-a", "outer", "-m", "outer", "inner"]);
    let inner_oid = git(worktree, &["rev-parse", "inner^{}"]).trim().to_owned();
    ObjectId::from_hex(&inner_oid).expect("inner")
}

fn assert_same(worktree: &std::path::Path, repo: &Repository, git_args: &[&str], query: RefQuery) {
    let git_names = git_refnames(worktree, git_args);
    let grit_names = grit_refnames(repo, query);
    assert_eq!(grit_names, git_names, "git {git_args:?}");
}

fn maybe_write_graph(worktree: &std::path::Path, repo: &Repository, with_graph: bool) {
    if with_graph {
        repo.write_commit_graph(&CommitGraphOptions::default())
            .expect("write commit-graph");
        git(worktree, &["commit-graph", "write"]);
    }
}

fn scenario_contains(backend: Backend, with_graph: bool) {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, backend);
    seed_linear(worktree);
    maybe_write_graph(worktree, &repo, with_graph);
    let mid = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD~1"]).trim()).expect("mid");
    assert_same(
        worktree,
        &repo,
        &["refs/tags/", &format!("--contains={mid}")],
        RefQuery {
            namespaces: vec![RefNamespace::Tags],
            contains: vec![mid],
            ..RefQuery::default()
        },
    );
}

fn scenario_points_at(backend: Backend, with_graph: bool) {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, backend);
    let inner = seed_tag_of_tag(worktree);
    maybe_write_graph(worktree, &repo, with_graph);
    assert_same(
        worktree,
        &repo,
        &["refs/tags/", &format!("--points-at={inner}")],
        RefQuery {
            namespaces: vec![RefNamespace::Tags],
            points_at: vec![inner],
            ..RefQuery::default()
        },
    );
}

fn scenario_merged(backend: Backend, with_graph: bool) {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, backend);
    let (merge_tip, _side) = seed_merge(worktree);
    maybe_write_graph(worktree, &repo, with_graph);
    assert_same(
        worktree,
        &repo,
        &["refs/heads/", &format!("--merged={merge_tip}")],
        RefQuery {
            namespaces: vec![RefNamespace::Heads],
            merged: vec![merge_tip],
            ..RefQuery::default()
        },
    );
}

#[test]
fn reachability_matches_git_no_graph() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    for backend in [Backend::Files, Backend::Reftable] {
        if backend == Backend::Reftable && !require_reftable_git() {
            continue;
        }
        scenario_contains(backend, false);
        scenario_points_at(backend, false);
        scenario_merged(backend, false);
    }
}

#[test]
fn reachability_matches_git_with_graph() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    for backend in [Backend::Files, Backend::Reftable] {
        if backend == Backend::Reftable && !require_reftable_git() {
            continue;
        }
        scenario_contains(backend, true);
        scenario_points_at(backend, true);
        scenario_merged(backend, true);
    }
}

#[test]
fn contains_rejects_non_commit_target() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(root.path(), &InitOptions::default()).expect("init");
    git(root.path(), &["commit", "--allow-empty", "-q", "-m", "c"]);
    let blob = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"x")
        .expect("blob");
    let err = resolve_commit_filter_target(&repo, blob).unwrap_err();
    assert!(matches!(err, RefQueryError::NotCommit(_)));
}

fn seed_two_roots(worktree: &std::path::Path) -> (ObjectId, ObjectId) {
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "root-a"]);
    let a = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("a");
    git(worktree, &["checkout", "--orphan", "other"]);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "root-b"]);
    let b = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("b");
    git(worktree, &["checkout", "-B", "master"]);
    git(
        worktree,
        &["merge", "--allow-unrelated-histories", "--no-edit", "other"],
    );
    (a, b)
}

#[test]
fn combined_contains_matches_git_on_two_roots() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    let (a, b) = seed_two_roots(worktree);

    for (extra, query) in [
        (
            vec![format!("--contains={a}"), format!("--contains={b}")],
            RefQuery {
                namespaces: vec![RefNamespace::Heads],
                contains: vec![a, b],
                ..RefQuery::default()
            },
        ),
        (
            vec![format!("--contains={a}"), format!("--no-contains={b}")],
            RefQuery {
                namespaces: vec![RefNamespace::Heads],
                contains: vec![a],
                no_contains: vec![b],
                ..RefQuery::default()
            },
        ),
    ] {
        let mut git_args = vec!["for-each-ref", "--format=%(refname)"];
        for e in &extra {
            git_args.push(e.as_str());
        }
        git_args.push("refs/heads/");
        let git_names = git_refnames(worktree, &git_args);
        let grit_names = grit_refnames(&repo, query);
        assert_eq!(grit_names, git_names, "git args {git_args:?}");
    }
}

fn git_commit_with_date(worktree: &std::path::Path, date: &str, msg: &str) {
    std::process::Command::new("git")
        .current_dir(worktree)
        .args(["commit", "--allow-empty", "-q", "-m", msg])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .expect("commit");
}

#[test]
fn contains_skewed_committer_dates_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    git_commit_with_date(worktree, "1893456000 +0000", "target");
    let target = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("target");
    for i in 0..101 {
        git_commit_with_date(worktree, "946684800 +0000", &format!("old-{i}"));
    }
    assert_same(
        worktree,
        &repo,
        &["refs/heads/", &format!("--contains={target}")],
        RefQuery {
            namespaces: vec![RefNamespace::Heads],
            contains: vec![target],
            ..RefQuery::default()
        },
    );
}

#[test]
fn points_at_deep_tag_chain_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "base"]);
    let inner = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("inner");
    let mut target = inner;
    for i in 0..35 {
        git(
            worktree,
            &[
                "tag",
                "-a",
                &format!("t{i}"),
                "-m",
                "m",
                &target.to_string(),
            ],
        );
        target = ObjectId::from_hex(git(worktree, &["rev-parse", &format!("t{i}")]).trim())
            .expect("tag");
    }
    assert_same(
        worktree,
        &repo,
        &["refs/tags/", &format!("--points-at={inner}")],
        RefQuery {
            namespaces: vec![RefNamespace::Tags],
            points_at: vec![inner],
            ..RefQuery::default()
        },
    );
}

#[test]
fn contains_missing_object_is_typed() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(root.path(), &InitOptions::default()).expect("init");
    let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
    let err = resolve_commit_filter_target(&repo, missing).unwrap_err();
    assert!(matches!(err, RefQueryError::MissingObject(_)));
}

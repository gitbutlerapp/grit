//! Ref query `--contains`, `--merged`, and `--points-at` parity with `git for-each-ref`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

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
    let repo = Repository::init(worktree, &opts).expect("init");
    if backend == Backend::Reftable {
        // Seed HEAD in the table too: system Git ignores the loose HEAD in a reftable repo.
        git(worktree, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }
    repo
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
fn shared_history_contains_batched_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "root"]);
    let target = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("target");
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "shared"]);
    let shared = git(worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    git(worktree, &["branch", "tip-a", "HEAD"]);
    git(worktree, &["branch", "tip-b", "HEAD"]);
    git(worktree, &["tag", "t-a", "tip-a"]);
    git(worktree, &["tag", "t-b", "tip-b"]);
    let query = RefQuery {
        namespaces: vec![RefNamespace::Tags],
        contains: vec![target],
        ..RefQuery::default()
    };
    let git_args = ["refs/tags/", &format!("--contains={target}")];
    assert_same(worktree, &repo, &git_args, query.clone());

    maybe_write_graph(worktree, &repo, true);
    assert_same(worktree, &repo, &git_args, query);
    let _ = shared;
}

/// Regression: date-based pruning with >64 unique tips returned 0 grit refs vs 65 from git.
#[test]
fn many_unique_tips_skewed_dates_contains_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    git_commit_with_date(worktree, "1893456000 +0000", "target");
    let target = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("target");
    for i in 0..80 {
        git_commit_with_date(worktree, "946684800 +0000", &format!("old-{i}"));
    }
    for i in 0..65 {
        let spec = format!("HEAD~{i}");
        git(worktree, &["tag", &format!("batch-{i}"), &spec]);
    }
    let query = RefQuery {
        namespaces: vec![RefNamespace::Tags],
        contains: vec![target],
        ..RefQuery::default()
    };
    let git_args = ["refs/tags/batch-*", &format!("--contains={target}")];
    let git_names = git_refnames(worktree, &git_args);
    let grit_names = grit_refnames(&repo, query.clone());
    assert_eq!(
        grit_names.len(),
        git_names.len(),
        "ref count: git={} grit={}",
        git_names.len(),
        grit_names.len()
    );
    assert_eq!(grit_names, git_names);
    assert!(
        git_names.len() >= 65,
        "oracle expects at least 65 matching tags, got {}",
        git_names.len()
    );

    maybe_write_graph(worktree, &repo, true);
    let grit_with_graph = grit_refnames(&repo, query.clone());
    let git_with_graph = git_refnames(worktree, &git_args);
    assert_eq!(grit_with_graph, git_with_graph);
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
fn contains_excludes_tree_tag_without_commit_tip() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "base"]);
    let tree = git(worktree, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();
    git(worktree, &["tag", "-a", "tree-tag", "-m", "tree", &tree]);
    git(worktree, &["tag", "lightweight-tree", &tree]);
    let target = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD"]).trim()).expect("target");
    assert_same(
        worktree,
        &repo,
        &["refs/tags/", &format!("--contains={target}")],
        RefQuery {
            namespaces: vec![RefNamespace::Tags],
            contains: vec![target],
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

#[derive(Debug)]
struct CountingObjects {
    inner: grit_lib::objects::odb::store::MemoryStore,
    reads: std::sync::Mutex<std::collections::HashMap<ObjectId, usize>>,
    info: std::sync::Mutex<std::collections::HashMap<ObjectId, usize>>,
}

impl CountingObjects {
    fn new() -> Self {
        Self {
            inner: grit_lib::objects::odb::store::MemoryStore::new(
                grit_lib::objects::HashAlgo::Sha1,
            ),
            reads: Default::default(),
            info: Default::default(),
        }
    }
}

impl grit_lib::objects::odb::store::ObjectStore for CountingObjects {
    fn hash_algo(&self) -> grit_lib::objects::HashAlgo {
        self.inner.hash_algo()
    }
    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<grit_lib::objects::Object>> {
        *self.reads.lock().unwrap().entry(*oid).or_default() += 1;
        self.inner.read(oid)
    }
    fn read_info(
        &self,
        oid: &ObjectId,
    ) -> grit_lib::error::Result<Option<grit_lib::objects::ObjectInfo>> {
        *self.info.lock().unwrap().entry(*oid).or_default() += 1;
        self.inner.read_info(oid)
    }
    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> std::ops::ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_object(f)
    }
}

impl grit_lib::objects::odb::store::WritableObjectStore for CountingObjects {
    fn freshen(&self, oid: &ObjectId) -> grit_lib::error::Result<bool> {
        self.inner.freshen(oid)
    }
    fn write(
        &self,
        kind: grit_lib::objects::ObjectKind,
        data: &[u8],
        opts: grit_lib::objects::odb::WriteOptions,
    ) -> grit_lib::error::Result<ObjectId> {
        self.inner.write(kind, data, opts)
    }
}

fn counted_repo(root: &std::path::Path, objects: std::sync::Arc<CountingObjects>) -> Repository {
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::objects::odb::OdbBuilder;
    use grit_lib::repo::RepositoryBuilder;
    init_repo(root, Backend::Files);
    RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(root.join(".git/objects"))
                .primary(objects)
                .alternates(false),
        )
        .open(&root.join(".git"), Some(root))
        .unwrap()
}

fn counted_commit(repo: &Repository, parent: Option<ObjectId>, n: usize) -> ObjectId {
    use grit_lib::objects::ObjectKind;
    let tree = repo.odb().write(ObjectKind::Tree, b"").unwrap();
    let parent = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
    repo.odb().write(ObjectKind::Commit, format!("tree {tree}\n{parent}author T <t@e.com> 1700000000 +0000\ncommitter T <t@e.com> 1700000000 +0000\n\nc{n}\n").as_bytes()).unwrap()
}

/// The reported fixture shape: unique recent tips first, then older nonmatching tips.
/// The limit must bound object work, rather than merely truncating the final output.
#[test]
fn bounded_contains_does_not_load_irrelevant_trailing_tips() {
    let root = tempfile::tempdir().unwrap();
    let objects = std::sync::Arc::new(CountingObjects::new());
    let repo = counted_repo(root.path(), objects.clone());
    let mut commits = Vec::new();
    for i in 0..1200 {
        commits.push(counted_commit(&repo, commits.last().copied(), i));
    }
    let target = commits[1099];
    let mut packed = String::from("# pack-refs with: sorted\n");
    for (i, tip) in commits.iter().rev().enumerate() {
        packed.push_str(&format!("{tip} refs/heads/dogfood/{i:05}\n"));
    }
    std::fs::write(repo.git_dir().join("packed-refs"), packed).unwrap();
    objects.reads.lock().unwrap().clear();
    objects.info.lock().unwrap().clear();
    let rows = query_refs(
        &repo,
        &RefQuery {
            contains: vec![target],
            limit: Some(100),
            ..RefQuery::default()
        },
        &RefQueryOptions::default(),
    )
    .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.full_name.clone()).collect::<Vec<_>>(),
        (0..100)
            .map(|i| format!("refs/heads/dogfood/{i:05}"))
            .collect::<Vec<_>>()
    );
    let reads = objects.reads.lock().unwrap();
    let info = objects.info.lock().unwrap();
    let irrelevant = commits[..1099]
        .iter()
        .filter(|id| reads.contains_key(id) || info.contains_key(id))
        .count();
    assert_eq!(
        irrelevant, 0,
        "bounded containment must not load older trailing tips"
    );
    assert!(
        reads.values().sum::<usize>() <= 102,
        "shared ancestry must be read once"
    );
}

/// Many refs share few tips (the `branch --merged` shape): each distinct tip is resolved
/// once, not once per ref and per filter stage.
#[test]
fn merged_filter_reads_each_shared_tip_once() {
    let root = tempfile::tempdir().unwrap();
    let objects = std::sync::Arc::new(CountingObjects::new());
    let repo = counted_repo(root.path(), objects.clone());
    let mut trunk = Vec::new();
    for i in 0..20 {
        trunk.push(counted_commit(&repo, trunk.last().copied(), i));
    }
    let side = counted_commit(&repo, Some(trunk[5]), 100);
    let base = trunk[14];
    let tips = [trunk[3], trunk[9], trunk[14], trunk[19], side];
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    let mut names = Vec::new();
    for (t, tip) in tips.iter().enumerate() {
        for alias in 0..40 {
            names.push((format!("refs/heads/t{t}/a{alias:02}"), *tip));
        }
    }
    names.sort();
    for (name, tip) in &names {
        packed.push_str(&format!("{tip} {name}\n"));
    }
    std::fs::write(repo.git_dir().join("packed-refs"), packed).unwrap();

    for (merged, expect_tips) in [(true, vec![0, 1, 2]), (false, vec![3, 4])] {
        objects.reads.lock().unwrap().clear();
        objects.info.lock().unwrap().clear();
        let query = if merged {
            RefQuery {
                merged: vec![base],
                ..RefQuery::default()
            }
        } else {
            RefQuery {
                no_merged: vec![base],
                ..RefQuery::default()
            }
        };
        let got = grit_refnames(&repo, query);
        let expected: Vec<String> = names
            .iter()
            .filter(|(_, tip)| expect_tips.iter().any(|&t| tips[t] == *tip))
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(got, expected, "merged={merged}");
        let info = objects.info.lock().unwrap();
        for tip in tips {
            let n = info.get(&tip).copied().unwrap_or(0);
            assert!(
                n <= 2,
                "tip {tip} header read {n} times for 40 refs (merged={merged})"
            );
        }
    }
}

/// `--merged` / `--no-merged` with many branches per tip, annotated tags, and a ref to a
/// tree, with loose and packed refs, match `git for-each-ref`.
#[test]
fn merged_with_shared_tips_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    let (merge_tip, side_tip) = seed_merge(worktree);
    for i in 0..30 {
        git(worktree, &["branch", &format!("alias/side-{i:02}"), "side"]);
        git(
            worktree,
            &["branch", &format!("alias/open-{i:02}"), "open-tip"],
        );
    }
    git(worktree, &["tag", "-a", "annot-side", "-m", "m", "side"]);
    let tree = git(worktree, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_owned();
    git(worktree, &["update-ref", "refs/tags/tree-ref", &tree]);
    for pack in [false, true] {
        if pack {
            git(worktree, &["pack-refs", "--all"]);
        }
        for (namespaces, prefix) in [
            (vec![RefNamespace::Heads], "refs/heads/"),
            (vec![RefNamespace::Tags], "refs/tags/"),
        ] {
            for target in [merge_tip, side_tip] {
                assert_same(
                    worktree,
                    &repo,
                    &[prefix, &format!("--merged={target}")],
                    RefQuery {
                        namespaces: namespaces.clone(),
                        merged: vec![target],
                        ..RefQuery::default()
                    },
                );
                assert_same(
                    worktree,
                    &repo,
                    &[prefix, &format!("--no-merged={target}")],
                    RefQuery {
                        namespaces: namespaces.clone(),
                        no_merged: vec![target],
                        ..RefQuery::default()
                    },
                );
            }
        }
    }
}

/// With too few refs for the `packed-refs` peel shortcut, the filter's tip resolution is
/// also the listing's tag peel: a branch tip's header is read once, not once to peel and
/// again to find its commit.
#[test]
fn merged_filter_peels_loose_tips_with_one_header_read() {
    let root = tempfile::tempdir().unwrap();
    let objects = std::sync::Arc::new(CountingObjects::new());
    let repo = counted_repo(root.path(), objects.clone());
    let mut trunk = Vec::new();
    for i in 0..12 {
        trunk.push(counted_commit(&repo, trunk.last().copied(), i));
    }
    let base = trunk[8];
    let tips = [trunk[2], trunk[5], trunk[11]];
    for (i, tip) in tips.iter().enumerate() {
        std::fs::write(
            repo.git_dir().join(format!("refs/heads/b{i}")),
            format!("{tip}\n"),
        )
        .unwrap();
    }
    objects.info.lock().unwrap().clear();
    let rows = query_refs(
        &repo,
        &RefQuery {
            merged: vec![base],
            ..RefQuery::default()
        },
        &RefQueryOptions::default(),
    )
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.full_name.as_str())
            .collect::<Vec<_>>(),
        ["refs/heads/b0", "refs/heads/b1"]
    );
    assert!(rows.iter().all(|r| r.peeled.is_none()));
    let info = objects.info.lock().unwrap();
    for tip in tips {
        let n = info.get(&tip).copied().unwrap_or(0);
        assert!(n <= 1, "tip {tip} header read {n} times");
    }
}

/// Listings that went through a reachability filter carry the full tag peel Git computes
/// (`<ref>^{}`, the `packed-refs` peel line) for annotated tags, loose and packed.
#[test]
fn filtered_listings_report_git_peels() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    let repo = init_repo(worktree, Backend::Files);
    let tip = seed_linear(worktree);
    seed_tag_of_tag(worktree);
    let tree = git(worktree, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_owned();
    git(worktree, &["tag", "-a", "tree-tag", "-m", "t", &tree]);
    let mid = ObjectId::from_hex(git(worktree, &["rev-parse", "HEAD~1"]).trim()).expect("mid");
    let all = vec![RefNamespace::Custom("refs/".to_owned())];
    for pack in [false, true] {
        if pack {
            git(worktree, &["pack-refs", "--all"]);
        }
        let cases: Vec<(Vec<String>, RefQuery)> = vec![
            (
                vec![format!("--merged={tip}")],
                RefQuery {
                    merged: vec![tip],
                    namespaces: all.clone(),
                    ..RefQuery::default()
                },
            ),
            (
                vec![format!("--no-merged={mid}")],
                RefQuery {
                    no_merged: vec![mid],
                    namespaces: all.clone(),
                    ..RefQuery::default()
                },
            ),
            (
                vec![format!("--points-at={mid}")],
                RefQuery {
                    points_at: vec![mid],
                    namespaces: all.clone(),
                    ..RefQuery::default()
                },
            ),
            (
                vec![format!("--contains={mid}"), "--count=2".to_owned()],
                RefQuery {
                    contains: vec![mid],
                    limit: Some(2),
                    namespaces: all.clone(),
                    ..RefQuery::default()
                },
            ),
        ];
        for (filter, query) in cases {
            let mut args = vec![
                "for-each-ref".to_owned(),
                "--format=%(refname) %(objecttype)".to_owned(),
            ];
            args.extend(filter.iter().cloned());
            args.push("refs/".to_owned());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let expected: Vec<String> = git(worktree, &args)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| match l.split_once(' ') {
                    Some((name, "tag")) => {
                        let peeled = git(worktree, &["rev-parse", &format!("{name}^{{}}")]);
                        format!("{name} {}", peeled.trim())
                    }
                    Some((name, _)) => name.to_owned(),
                    None => l.to_owned(),
                })
                .collect();
            let got: Vec<String> = query_refs(&repo, &query, &RefQueryOptions::default())
                .expect("query_refs")
                .into_iter()
                .map(|r| match r.peeled {
                    Some(p) => format!("{} {p}", r.full_name),
                    None => r.full_name,
                })
                .collect();
            assert_eq!(got, expected, "pack={pack} {filter:?}");
        }
    }
}

#[test]
fn bounded_contains_memoizes_shared_negative_history() {
    let root = tempfile::tempdir().unwrap();
    let objects = std::sync::Arc::new(CountingObjects::new());
    let repo = counted_repo(root.path(), objects.clone());
    let target = counted_commit(&repo, None, 10000);
    let mut trunk = Vec::new();
    for i in 0..300 {
        trunk.push(counted_commit(&repo, trunk.last().copied(), i));
    }
    let mut packed = String::from("# pack-refs with: sorted\n");
    for i in 0..100 {
        let tip = counted_commit(&repo, trunk.last().copied(), 1000 + i);
        packed.push_str(&format!("{tip} refs/heads/a{i:03}\n"));
    }
    packed.push_str(&format!("{target} refs/heads/z-match\n"));
    std::fs::write(repo.git_dir().join("packed-refs"), packed).unwrap();
    objects.reads.lock().unwrap().clear();
    let rows = query_refs(
        &repo,
        &RefQuery {
            contains: vec![target],
            limit: Some(1),
            ..RefQuery::default()
        },
        &RefQueryOptions::default(),
    )
    .unwrap();
    assert_eq!(rows[0].short_name, "z-match");
    let reads = objects.reads.lock().unwrap();
    for oid in trunk {
        assert_eq!(reads.get(&oid).copied().unwrap_or(0), 1);
    }
}

/// Core t3201-style containment combined with bounded, ordered ref selection.
#[test]
fn bounded_contains_order_filters_tags_and_backends_match_git() {
    use grit_lib::refs::listing::{RefSort, SortKey};
    for backend in [Backend::Files, Backend::Reftable] {
        if !git_interop_available(backend) {
            continue;
        }
        for with_graph in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let worktree = root.path();
            let repo = init_repo(worktree, backend);
            git_commit_with_date(worktree, "946684800 +0000", "root");
            let old = git(worktree, &["rev-parse", "HEAD"]).trim().to_owned();
            git(worktree, &["branch", "test/a-nonmatch", &old]);
            git_commit_with_date(worktree, "1893456000 +0000", "target");
            let target: ObjectId = git(worktree, &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .unwrap();
            git(worktree, &["branch", "test/c-target"]);
            git(worktree, &["checkout", "-b", "side"]);
            git_commit_with_date(worktree, "946684800 +0000", "side");
            let side: ObjectId = git(worktree, &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .unwrap();
            git(worktree, &["checkout", "main"]);
            for i in 0..12 {
                git_commit_with_date(worktree, "946684800 +0000", &format!("recent-{i}"));
                git(worktree, &["branch", &format!("test/r{i}"), "HEAD"]);
            }
            git(worktree, &["merge", "--no-ff", "--no-edit", "side"]);
            git(worktree, &["branch", "test/b-merge"]);
            git(worktree, &["branch", "test/d-shared"]);
            git(worktree, &["branch", "test/e-nonmatch", &old]);
            git(
                worktree,
                &["tag", "-a", "test/tag-inner", "-m", "inner", "HEAD"],
            );
            git(
                worktree,
                &[
                    "tag",
                    "-a",
                    "test/tag-outer",
                    "-m",
                    "outer",
                    "test/tag-inner",
                ],
            );
            let tree = git(worktree, &["rev-parse", "HEAD^{tree}"])
                .trim()
                .to_owned();
            git(worktree, &["tag", "test/tag-tree", &tree]);
            if backend == Backend::Files {
                git(worktree, &["pack-refs", "--all"]);
            }
            maybe_write_graph(worktree, &repo, with_graph);
            let base = RefQuery {
                namespaces: vec![
                    RefNamespace::Heads,
                    RefNamespace::Tags,
                    RefNamespace::Custom("refs/heads/".into()),
                ],
                patterns: vec!["test/".into()],
                contains: vec![target],
                ..RefQuery::default()
            };
            for sort in [SortKey::Name, SortKey::Version, SortKey::Date] {
                for reverse in [false, true] {
                    for filter in 0..6 {
                        let mut query = base.clone();
                        query.sort = RefSort { key: sort, reverse };
                        let key = match sort {
                            SortKey::Name => "refname",
                            SortKey::Version => "version:refname",
                            SortKey::Date => "creatordate",
                        };
                        let sort_arg = format!("--sort={}{key}", if reverse { "-" } else { "" });
                        let mut args = vec![sort_arg, format!("--contains={target}")];
                        match filter {
                            1 => {
                                query.no_contains = vec![side];
                                args.push(format!("--no-contains={side}"));
                            }
                            2 => {
                                query.merged = vec![target];
                                args.push(format!("--merged={target}"));
                            }
                            3 => {
                                query.no_merged = vec![target];
                                args.push(format!("--no-merged={target}"));
                            }
                            4 => {
                                query.points_at = vec![target];
                                args.push(format!("--points-at={target}"));
                            }
                            5 => {
                                query.contains.push(old.parse().unwrap());
                                args.push(format!("--contains={old}"));
                            }
                            _ => {}
                        }
                        args.extend(["refs/heads/test/".into(), "refs/tags/test/".into()]);
                        // Date ties have a name tie-break in both implementations.
                        for limit in [Some(0), Some(1), Some(3), Some(100), None] {
                            query.limit = limit;
                            let mut expected = git_refnames(
                                worktree,
                                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                            );
                            if let Some(limit) = limit {
                                expected.truncate(limit);
                            }
                            assert_eq!(grit_refnames(&repo, query.clone()), expected,
                                "{backend:?}, graph={with_graph}, sort={sort:?}, reverse={reverse}, filter={filter}, limit={limit:?}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn bounded_contains_validates_targets_even_with_zero_limit_or_no_candidates() {
    let root = tempfile::tempdir().unwrap();
    let repo = init_repo(root.path(), Backend::Files);
    let blob = repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"x")
        .unwrap();
    let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
    for limit in [0, 1] {
        for target in [blob, missing] {
            let err = query_refs(
                &repo,
                &RefQuery {
                    contains: vec![target],
                    limit: Some(limit),
                    ..RefQuery::default()
                },
                &RefQueryOptions::default(),
            )
            .unwrap_err();
            match target {
                t if t == blob => assert!(matches!(err, RefQueryError::NotCommit(_))),
                _ => assert!(matches!(err, RefQueryError::MissingObject(_))),
            }
        }
    }
}

#[test]
fn bounded_contains_parent_rewrite_cycles_match_unlimited_and_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    for backend in [Backend::Files, Backend::Reftable] {
        if backend == Backend::Reftable && !require_reftable_git() {
            continue;
        }
        for with_graph in [false, true] {
            // Replacement lookup currently reads loose refs only; reftable coverage
            // here exercises graft cycles, which both implementations support.
            let rewrites = if backend == Backend::Files {
                &[false, true][..]
            } else {
                &[false][..]
            };
            for &replace in rewrites {
                let dir = tempfile::tempdir().unwrap();
                let worktree = dir.path();
                let repo = init_repo(worktree, backend);
                let tree = repo
                    .odb()
                    .write(grit_lib::objects::ObjectKind::Tree, b"")
                    .unwrap();
                let root = git(worktree, &["commit-tree", &tree.to_string(), "-m", "root"])
                    .trim()
                    .to_owned();
                let child = git(
                    worktree,
                    &["commit-tree", &tree.to_string(), "-p", &root, "-m", "child"],
                )
                .trim()
                .to_owned();
                git(worktree, &["update-ref", "refs/heads/a-root", &root]);
                git(worktree, &["update-ref", "refs/heads/b-child", &child]);
                maybe_write_graph(worktree, &repo, with_graph);
                if replace {
                    let replacement = git(
                        worktree,
                        &[
                            "commit-tree",
                            &tree.to_string(),
                            "-p",
                            &child,
                            "-m",
                            "replacement",
                        ],
                    )
                    .trim()
                    .to_owned();
                    git(
                        worktree,
                        &["update-ref", &format!("refs/replace/{root}"), &replacement],
                    );
                } else {
                    std::fs::create_dir_all(repo.git_dir().join("info")).unwrap();
                    std::fs::write(
                        repo.git_dir().join("info/grafts"),
                        format!("{root} {child}\n"),
                    )
                    .unwrap();
                }
                // The graph intentionally retains the original parents. Check object
                // integrity without asking fsck to validate that stale acceleration.
                git(
                    worktree,
                    &[
                        "-c",
                        "core.commitGraph=false",
                        "fsck",
                        "--strict",
                        "--no-progress",
                    ],
                );
                // Reopen after changing ancestry so the query sees the parent rewrites.
                let repo = Repository::open(
                    repo.git_dir(),
                    &grit_lib::config::environment::RepositoryOptions::empty(),
                )
                .unwrap();
                for reverse in [false, true] {
                    let sort = if reverse {
                        "--sort=-refname"
                    } else {
                        "--sort=refname"
                    };
                    let all = git_refnames(
                        worktree,
                        &[sort, &format!("--contains={child}"), "refs/heads/"],
                    );
                    let expected = if reverse {
                        vec!["refs/heads/b-child", "refs/heads/a-root"]
                    } else {
                        vec!["refs/heads/a-root", "refs/heads/b-child"]
                    };
                    assert_eq!(all, expected);
                    for limit in [None, Some(1), Some(2), Some(100)] {
                        let query = RefQuery {
                            namespaces: vec![RefNamespace::Heads],
                            contains: vec![child.parse().unwrap()],
                            sort: grit_lib::refs::listing::RefSort {
                                key: grit_lib::refs::listing::SortKey::Name,
                                reverse,
                            },
                            limit,
                            ..RefQuery::default()
                        };
                        assert_eq!(grit_refnames(&repo, query), all[..limit.unwrap_or(all.len()).min(all.len())],
                            "backend={backend:?} graph={with_graph} replace={replace} reverse={reverse} limit={limit:?}");
                    }
                }
            }
        }
    }
}

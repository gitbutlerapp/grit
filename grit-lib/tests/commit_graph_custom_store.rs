//! Commit-graphs for custom primary object stores.
//!
//! A custom primary never trusts graph files under `objects/info/` (they describe the files
//! layout, not the store), and a graph built with `build_commit_graph` and attached with
//! `OdbBuilder::commit_graph` gives its walks the same answers as the files backend without
//! reading commit objects. Graph bytes are cross-checked with the system `git`.

use grit_lib::error::ObjectError;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, Result};
use grit_lib::maintenance::{CommitGraphBuild, CommitGraphOptions};
use grit_lib::objects::commit_graph::CommitGraphChain;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::repo::{Repository, RepositoryError};
use grit_lib::revwalk::RevWalkListOptions;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};
use grit_test_support::{git, git_cmd, rev_parse};

/// Memory store that counts how often a commit object is read, by either read method.
#[derive(Debug)]
struct CountingStore {
    inner: MemoryStore,
    commit_reads: AtomicUsize,
}

impl CountingStore {
    fn new(algo: HashAlgo) -> Self {
        Self {
            inner: MemoryStore::new(algo),
            commit_reads: AtomicUsize::new(0),
        }
    }

    fn take_commit_reads(&self) -> usize {
        self.commit_reads.swap(0, Ordering::Relaxed)
    }

    fn count(&self, kind: Option<ObjectKind>) {
        if kind == Some(ObjectKind::Commit) {
            self.commit_reads.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl ObjectStore for CountingStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let obj = self.inner.read(oid)?;
        self.count(obj.as_ref().map(|o| o.kind));
        Ok(obj)
    }

    fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        let kind = self.inner.read_into(oid, buf)?;
        self.count(kind);
        Ok(kind)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.inner.contains(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.inner.for_each_object(f)
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        self.inner.lookup_prefix(prefix, limit, out)
    }
}

impl WritableObjectStore for CountingStore {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        self.inner.freshen(oid)
    }
}

/// `main` gets `commits` commits, each editing one of 15 files under five directories; a
/// three-commit `side` branch forks at commit 10 and is merged back at commit 20.
fn history_repo(dir: &Path, commits: usize, object_format: &str) {
    git(
        dir,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={object_format}"),
        ],
    );
    let mut stream = String::new();
    let commit = |stream: &mut String, branch: &str, mark: usize, from: Option<usize>| {
        let message = format!("c{mark}\n");
        stream.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{mark}\ncommitter T <t@example.com> {} +0000\ndata {}\n{message}",
            1_600_000_000 + mark * 60,
            message.len()
        ));
        if let Some(from) = from {
            stream.push_str(&format!("from :{from}\n"));
        }
    };
    for i in 1..=commits {
        commit(&mut stream, "main", i, (i > 1).then(|| i - 1));
        if i == 20 {
            stream.push_str(&format!("merge :{}\n", 1_000 + 3));
        }
        let body = format!("v{i}\n");
        stream.push_str(&format!(
            "M 100644 inline dir{}/f{}.txt\ndata {}\n{body}\n",
            i % 5,
            i % 3,
            body.len()
        ));
        if i == 10 {
            for s in 1..=3 {
                let mark = 1_000 + s;
                let from = if s == 1 { 10 } else { mark - 1 };
                commit(&mut stream, "side", mark, Some(from));
                let body = format!("side{s}\n");
                stream.push_str(&format!(
                    "M 100644 inline side/s.txt\ndata {}\n{body}\n",
                    body.len()
                ));
            }
        }
    }
    git_cmd(&["fast-import", "--quiet"])
        .in_dir(dir)
        .stdin(stream)
        .suc();
}

fn files_repo(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open files repo")
}

/// Every object of the files repository, or only those `keep` accepts.
fn seeded_store(files: &Repository, keep: impl Fn(&Object, &ObjectId) -> bool) -> CountingStore {
    let store = CountingStore::new(files.odb().hash_algo());
    let source = files.odb().primary().expect("files primary");
    let mut oids = Vec::new();
    source
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("list objects");
    for oid in oids {
        let obj = source.read(&oid).expect("read").expect("listed object");
        if keep(&obj, &oid) {
            store
                .write(obj.kind, &obj.data, WriteOptions::default())
                .expect("copy object");
        }
    }
    store
}

fn open_custom(
    dir: &Path,
    store: Arc<CountingStore>,
    graph: Option<Arc<CommitGraphChain>>,
) -> Result<Repository> {
    let git_dir = dir.join(".git");
    let mut builder = OdbBuilder::new();
    if let Some(graph) = graph {
        builder = builder.commit_graph(graph);
    }
    Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store)
            .with_odb_builder(builder)
            .with_work_tree(dir)),
    )
}

fn built_graph(repo: &Repository, changed_paths: bool) -> grit_lib::maintenance::BuiltCommitGraph {
    match repo
        .build_commit_graph(&CommitGraphOptions::with_changed_paths(changed_paths))
        .expect("build commit-graph")
    {
        CommitGraphBuild::Built(built) => built,
        CommitGraphBuild::Skipped(reason) => panic!("unexpected skip: {reason:?}"),
    }
}

fn git_count(dir: &Path, range: &str) -> usize {
    git(dir, &["rev-list", "--count", range])
        .trim()
        .parse()
        .expect("count")
}

fn path_log(repo: &Repository, path: &str) -> Vec<ObjectId> {
    let options = RevWalkListOptions {
        limit: Some(5),
        pathspec: Pathspec::scoped(vec![path.to_owned()], PathspecGlobalFlags::default()),
        ..RevWalkListOptions::default()
    };
    repo.revwalk()
        .commits(&["main"], &options)
        .expect("log -- path")
}

#[test]
fn graph_file_under_objects_does_not_invent_commits_for_a_custom_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    history_repo(dir.path(), 12, "sha1");
    git(dir.path(), &["commit-graph", "write", "--reachable"]);
    let files = files_repo(dir.path());
    assert_eq!(
        files
            .revwalk()
            .count(&["main"], &RevWalkListOptions::default())
            .expect("files"),
        git_count(dir.path(), "main")
    );

    // The store holds the last three commits of `main` and every tree and blob; the graph
    // file on disk describes all of `main`'s history.
    let kept: Vec<ObjectId> = (0..3)
        .map(|n| rev_parse(dir.path(), &format!("main~{n}")))
        .collect();
    let store = Arc::new(seeded_store(&files, |obj, oid| {
        obj.kind != ObjectKind::Commit || kept.contains(oid)
    }));
    let custom = open_custom(dir.path(), Arc::clone(&store), None).expect("open");

    let counted = custom
        .revwalk()
        .count(&["main"], &RevWalkListOptions::default());
    assert!(
        matches!(counted, Err(Error::Object(ObjectError::NotFound { .. }))),
        "the walk must reach the store's missing parent, not take it from the graph file: {counted:?}"
    );
    let bounded = RevWalkListOptions {
        limit: Some(3),
        ..RevWalkListOptions::default()
    };
    assert_eq!(
        custom
            .revwalk()
            .commits(&["main"], &bounded)
            .expect("bounded walk"),
        kept
    );
    assert!(
        store.take_commit_reads() > 0,
        "commits come from the store, not from the graph file"
    );
}

#[test]
fn attached_graph_answers_walks_like_the_files_backend_without_commit_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    history_repo(dir.path(), 60, "sha1");
    let files = files_repo(dir.path());
    let store = Arc::new(seeded_store(&files, |_, _| true));
    let plain = open_custom(dir.path(), Arc::clone(&store), None).expect("open");

    let built = built_graph(&plain, true);
    assert_eq!(built.commit_count, git_count(dir.path(), "--all"));
    assert!(built.bloom_stats.is_some_and(|s| s.filter_computed > 0));
    let graph = Arc::new(built.to_chain().expect("parse built graph"));
    let custom = open_custom(dir.path(), Arc::clone(&store), Some(graph)).expect("open");
    store.take_commit_reads();

    // Resolving `main~40` peels the tip it navigates from, which reads that one object;
    // the walks themselves read no commit.
    let all = RevWalkListOptions::default();
    for (range, endpoint_reads) in [("main", 0), ("main~40..main", 1), ("side..main", 0)] {
        let expected = git_count(dir.path(), range);
        assert_eq!(
            files.revwalk().count(&[range], &all).expect("files"),
            expected
        );
        assert_eq!(
            custom.revwalk().count(&[range], &all).expect("custom"),
            expected
        );
        assert_eq!(
            store.take_commit_reads(),
            endpoint_reads,
            "commit reads for {range}"
        );
    }

    let tip = rev_parse(dir.path(), "main~5");
    let other = rev_parse(dir.path(), "side~1");
    let expected = vec![rev_parse(
        dir.path(),
        git(dir.path(), &["merge-base", "main~5", "side~1"]).trim(),
    )];
    assert_eq!(
        files
            .revwalk()
            .merge_bases(&[tip, other], grit_lib::revwalk::MergeBaseMode::Best)
            .expect("files"),
        expected
    );
    assert_eq!(
        custom
            .revwalk()
            .merge_bases(&[tip, other], grit_lib::revwalk::MergeBaseMode::Best)
            .expect("custom"),
        expected
    );
    assert_eq!(
        store.take_commit_reads(),
        0,
        "merge-base reads no commit object"
    );

    for path in ["dir1/f1.txt", "side/s.txt"] {
        assert_eq!(
            path_log(&custom, path),
            path_log(&files, path),
            "log -- {path}"
        );
    }
}

#[test]
fn commits_newer_than_the_attached_graph_are_still_walked() {
    let dir = tempfile::tempdir().expect("tempdir");
    history_repo(dir.path(), 30, "sha1");
    let files = files_repo(dir.path());
    let store = Arc::new(seeded_store(&files, |_, _| true));
    let plain = open_custom(dir.path(), Arc::clone(&store), None).expect("open");
    let graph = Arc::new(built_graph(&plain, false).to_chain().expect("chain"));

    let expected = git_count(dir.path(), "main") + 2;
    let mut tip = rev_parse(dir.path(), "main");
    let tree = rev_parse(dir.path(), "main^{tree}");
    for n in 0..2 {
        let body = format!(
            "tree {tree}\nparent {tip}\nauthor T <t@example.com> {0} +0000\ncommitter T <t@example.com> {0} +0000\n\nafter graph {n}\n",
            1_700_000_000 + n
        );
        tip = plain
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
    }
    grit_test_support::refs::set_ref(&plain, "refs/heads/main", &tip).expect("move main");

    let custom = open_custom(dir.path(), Arc::clone(&store), Some(graph)).expect("open");
    store.take_commit_reads();
    assert_eq!(
        custom
            .revwalk()
            .count(&["main"], &RevWalkListOptions::default())
            .expect("count"),
        expected
    );
    assert_eq!(
        store.take_commit_reads(),
        2,
        "only the commits the graph does not cover are read"
    );
}

#[test]
fn built_graph_is_byte_identical_to_the_files_writer_and_verified_by_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    history_repo(dir.path(), 40, "sha1");
    let files = files_repo(dir.path());
    let store = Arc::new(seeded_store(&files, |_, _| true));
    let custom = open_custom(dir.path(), store, None).expect("open");
    let graph_path = dir.path().join(".git/objects/info/commit-graph");

    for changed_paths in [false, true] {
        let built = built_graph(&custom, changed_paths);
        files
            .write_commit_graph(&CommitGraphOptions::with_changed_paths(changed_paths))
            .expect("files write");
        assert_eq!(
            std::fs::read(&graph_path).expect("graph file"),
            built.bytes,
            "changed_paths={changed_paths}"
        );

        std::fs::write(&graph_path, &built.bytes).expect("install built bytes");
        git(dir.path(), &["commit-graph", "verify"]);
        git(dir.path(), &["fsck", "--no-dangling"]);
        let reopened = files_repo(dir.path());
        for range in ["main", "main~25..main", "side..main"] {
            assert_eq!(
                reopened
                    .revwalk()
                    .count(&[range], &RevWalkListOptions::default())
                    .expect("grit count"),
                git_count(dir.path(), range),
                "{range} through the installed graph"
            );
        }
    }
}

#[test]
fn graph_written_by_git_attaches_to_a_custom_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    history_repo(dir.path(), 25, "sha1");
    git(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let files = files_repo(dir.path());
    let bytes = std::fs::read(dir.path().join(".git/objects/info/commit-graph")).expect("graph");
    let graph = Arc::new(CommitGraphChain::from_bytes(bytes).expect("git graph"));
    assert_eq!(graph.hash_algo(), HashAlgo::Sha1);
    assert!(graph.top_layer_bloom_settings().is_some());

    let store = Arc::new(seeded_store(&files, |_, _| true));
    let custom = open_custom(dir.path(), Arc::clone(&store), Some(graph)).expect("open");
    store.take_commit_reads();
    assert_eq!(
        custom
            .revwalk()
            .count(&["main"], &RevWalkListOptions::default())
            .expect("count"),
        git_count(dir.path(), "main")
    );
    assert_eq!(store.take_commit_reads(), 0);
    assert_eq!(
        path_log(&custom, "dir2/f2.txt"),
        path_log(&files, "dir2/f2.txt")
    );

    // A split layer depends on its base graphs and cannot stand alone.
    let commit_body = |parent: ObjectId| {
        let tree = rev_parse(dir.path(), "main^{tree}");
        format!(
            "tree {tree}\nparent {parent}\nauthor T <t@example.com> 1700000000 +0000\ncommitter T <t@example.com> 1700000000 +0000\n\nsplit\n"
        )
    };
    git(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let next = files
        .odb()
        .write(
            ObjectKind::Commit,
            commit_body(rev_parse(dir.path(), "main")).as_bytes(),
        )
        .expect("commit");
    grit_test_support::refs::set_ref(&files, "refs/heads/main", &next).expect("ref");
    git(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split=no-merge"],
    );
    let chain_file = dir
        .path()
        .join(".git/objects/info/commit-graphs/commit-graph-chain");
    let chain = std::fs::read_to_string(&chain_file).expect("chain file");
    let tip_layer = chain.lines().last().expect("tip layer");
    let layer = std::fs::read(dir.path().join(format!(
        ".git/objects/info/commit-graphs/graph-{tip_layer}.graph"
    )))
    .expect("tip layer file");
    assert!(chain.lines().count() > 1, "{chain}");
    assert!(matches!(
        CommitGraphChain::from_bytes(layer),
        Err(Error::Object(ObjectError::Corrupt { .. }))
    ));
}

#[test]
fn attached_graph_of_another_object_format_is_refused_on_open() {
    if !grit_test_support::git_supports_sha256() {
        eprintln!("SKIP: git lacks sha256");
        return;
    }
    let sha256 = tempfile::tempdir().expect("tempdir");
    history_repo(sha256.path(), 5, "sha256");
    git(sha256.path(), &["commit-graph", "write", "--reachable"]);
    let bytes = std::fs::read(sha256.path().join(".git/objects/info/commit-graph")).expect("graph");
    let graph = Arc::new(CommitGraphChain::from_bytes(bytes).expect("sha256 graph"));
    assert_eq!(graph.hash_algo(), HashAlgo::Sha256);

    let sha1 = tempfile::tempdir().expect("tempdir");
    history_repo(sha1.path(), 5, "sha1");
    let store = Arc::new(CountingStore::new(HashAlgo::Sha1));
    assert!(matches!(
        open_custom(sha1.path(), store, Some(graph)),
        Err(Error::Repository(RepositoryError::ObjectFormatMismatch {
            repository: HashAlgo::Sha1,
            store: HashAlgo::Sha256,
            ..
        }))
    ));
}

#[test]
fn octopus_graph_without_its_edge_chunk_is_rejected_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let mut stream = String::new();
    for (mark, branch) in [(1, "a"), (2, "b"), (3, "c")] {
        stream.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{mark}\ncommitter T <t@example.com> {} +0000\ndata 2\n{branch}\n\n",
            1_600_000_000 + mark
        ));
    }
    stream.push_str(
        "commit refs/heads/main\nmark :4\ncommitter T <t@example.com> 1600000010 +0000\ndata 8\noctopus\nfrom :1\nmerge :2\nmerge :3\n\n",
    );
    git_cmd(&["fast-import", "--quiet"])
        .in_dir(dir.path())
        .stdin(stream)
        .suc();
    git(dir.path(), &["commit-graph", "write", "--reachable"]);
    let graph_path = dir.path().join(".git/objects/info/commit-graph");
    let bytes = std::fs::read(&graph_path).expect("graph");
    let chain = CommitGraphChain::from_bytes(bytes.clone()).expect("git octopus graph");
    assert_eq!(chain.total_commits(), 4);

    let mut mutated = bytes;
    let edge_entry = (0..mutated[6] as usize)
        .map(|i| 8 + i * 12)
        .find(|&e| &mutated[e..e + 4] == b"EDGE")
        .expect("octopus graph has an EDGE chunk");
    mutated[edge_entry..edge_entry + 4].copy_from_slice(b"EDGX");
    let body_len = mutated.len() - HashAlgo::Sha1.len();
    let trailer = HashAlgo::Sha1.checksum_digest(&mutated[..body_len]);
    mutated[body_len..].copy_from_slice(&trailer);

    std::fs::remove_file(&graph_path).expect("git writes the graph read-only");
    std::fs::write(&graph_path, &mutated).expect("install mutated graph");
    let verify = git_cmd(&["commit-graph", "verify"])
        .in_dir(dir.path())
        .exec();
    assert!(!verify.ok(), "git must reject the graph");
    assert!(verify.stderr.contains("extra-edges"), "{}", verify.stderr);
    assert!(matches!(
        CommitGraphChain::from_bytes(mutated),
        Err(Error::Object(ObjectError::Corrupt { .. }))
    ));
}

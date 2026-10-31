//! Object-store round trips made by a one-file commit on a custom primary store.
//!
//! A database- or network-backed [`ObjectStore`] pays per call, so committing one
//! changed file must not probe every unchanged cached directory one by one: the
//! cache-tree reuse check asks about them in one batch
//! ([`ObjectStore::contains_many`]) and subtrees it has just confirmed or written are
//! not probed again. The same holds when history lives in a slow read source behind an
//! empty local primary ([`OdbBuilder::push_read_source`]). The objects grit writes are
//! then checked with the system `git`.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Result;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::stash::{StashMode, StashPushOptions};

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";
const FILES: usize = 1000;

/// Counts calls into a [`MemoryStore`]. With `batched`, `contains_many` is one call
/// (like a database store answering a whole batch in one query); without it, the
/// trait's default implementation runs one `contains` per id.
#[derive(Debug)]
struct CountingStore {
    inner: MemoryStore,
    batched: bool,
    contains: AtomicU64,
    contains_many: AtomicU64,
    writes: AtomicU64,
}

impl CountingStore {
    fn new(batched: bool) -> Self {
        Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            batched,
            contains: AtomicU64::new(0),
            contains_many: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> Calls {
        Calls {
            contains: self.contains.load(Ordering::Relaxed),
            contains_many: self.contains_many.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
        }
    }
}

impl ObjectStore for CountingStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.contains.fetch_add(1, Ordering::Relaxed);
        self.inner.contains(oid)
    }

    fn contains_many(&self, oids: &[ObjectId]) -> Result<Vec<bool>> {
        if !self.batched {
            return oids.iter().map(|oid| self.contains(oid)).collect();
        }
        self.contains_many.fetch_add(1, Ordering::Relaxed);
        self.inner.contains_many(oids)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for CountingStore {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        self.inner.freshen(oid)
    }
}

#[derive(Debug, Clone, Copy)]
struct Calls {
    contains: u64,
    contains_many: u64,
    writes: u64,
}

impl Calls {
    fn since(self, before: Calls) -> Calls {
        Calls {
            contains: self.contains - before.contains,
            contains_many: self.contains_many - before.contains_many,
            writes: self.writes - before.writes,
        }
    }

    fn round_trips(self) -> u64 {
        self.contains + self.contains_many
    }
}

fn commit_all(repo: &Repository, message: &str) {
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    commit_with_scope(
        repo,
        &{
            let mut o = CommitRequest::default();
            o.message = message.to_owned();
            o.author = Some(IDENT.parse().expect("signature"));
            o.committer = Some(IDENT.parse().expect("signature"));
            o
        },
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");
}

struct OneFileCommit {
    dir: tempfile::TempDir,
    store: Arc<CountingStore>,
    calls: Calls,
}

/// Commit `FILES` files spread over `dirs` directories, change one file, commit again,
/// and record the store calls the second commit made.
fn one_file_commit(dirs: usize, batched: bool) -> OneFileCommit {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let store = Arc::new(CountingStore::new(batched));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(&root))
        .expect("open");
    for d in 0..dirs {
        fs::create_dir_all(root.join(format!("d{d}"))).expect("mkdir");
    }
    for i in 0..FILES {
        fs::write(root.join(format!("d{}/f{i}", i % dirs)), format!("{i}\n")).expect("write");
    }
    commit_all(&repo, "initial");
    fs::write(root.join("d0/f0"), "changed\n").expect("modify");
    let before = store.snapshot();
    commit_all(&repo, "touch one file");
    let calls = store.snapshot().since(before);
    eprintln!("dirs={dirs} batched={batched}: {calls:?}");
    OneFileCommit { dir, store, calls }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Copy every object of `store` into the on-disk `objects/` under `root`.
fn export_to_disk(root: &Path, store: &MemoryStore) {
    let disk = Odb::new(&root.join(".git/objects"));
    let mut oids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("enumerate");
    for oid in oids {
        let obj = store.read(&oid).expect("read").expect("present");
        assert_eq!(disk.write(obj.kind, &obj.data).expect("export"), oid);
    }
}

/// Copy the custom store into the on-disk `objects/`, then let the system `git` verify
/// the history and confirm the committed tree matches the work tree and the index.
fn assert_git_accepts(commit: &OneFileCommit) {
    let root = commit.dir.path();
    export_to_disk(root, &commit.store.inner);
    git(root, &["fsck", "--full", "--strict"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert_eq!(git(root, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert_eq!(git(root, &["show", "HEAD:d0/f0"]), "changed\n");
    assert_eq!(
        git(root, &["diff", "--name-only", "HEAD~1", "HEAD"]).trim(),
        "d0/f0"
    );
}

/// A store answering `contains_many` in one call sees a bounded number of round trips,
/// however many unchanged directories the commit reuses.
#[test]
fn one_file_commit_round_trips_do_not_grow_with_directory_count() {
    for dirs in [10, 1000] {
        let commit = one_file_commit(dirs, true);
        // The changed blob, the two trees on its path and the commit.
        assert_eq!(commit.calls.writes, 4, "dirs={dirs}: {:?}", commit.calls);
        assert!(
            commit.calls.round_trips() <= 16,
            "dirs={dirs}: {:?}",
            commit.calls
        );
        assert_git_accepts(&commit);
    }
}

/// With the default one-id-at-a-time `contains_many`, each cached directory is probed
/// once, not once for the reuse check and again as a member of the rebuilt root.
#[test]
fn one_file_commit_probes_each_unchanged_directory_once() {
    let dirs = 1000;
    let commit = one_file_commit(dirs, false);
    assert_eq!(commit.calls.writes, 4, "{:?}", commit.calls);
    assert_eq!(commit.calls.contains_many, 0);
    assert!(
        commit.calls.contains <= dirs as u64 + 16,
        "{:?}",
        commit.calls
    );
    assert_git_accepts(&commit);
}

/// A repository whose history lives in a slow read source behind an empty local
/// primary (a read-through cache over a remote store).
struct Layered {
    dir: tempfile::TempDir,
    primary: Arc<MemoryStore>,
    remote: Arc<CountingStore>,
    repo: Repository,
}

/// Commit `FILES` files spread over `dirs` directories into a counting store, then
/// reopen the repository with an empty local primary and that store as the only
/// [`OdbBuilder::push_read_source`].
fn layered_repo(dirs: usize) -> Layered {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let remote = Arc::new(CountingStore::new(true));
    let open = |builder: OdbBuilder| {
        RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(builder.alternates(false))
            .open(&git_dir, Some(&root))
            .expect("open")
    };
    let history = open(OdbBuilder::files(git_dir.join("objects")).primary(remote.clone()));
    for d in 0..dirs {
        fs::create_dir_all(root.join(format!("d{d}"))).expect("mkdir");
    }
    for i in 0..FILES {
        fs::write(root.join(format!("d{}/f{i}", i % dirs)), format!("{i}\n")).expect("write");
    }
    commit_all(&history, "initial");
    drop(history);
    let primary = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = open(
        OdbBuilder::files(git_dir.join("objects"))
            .primary(primary.clone())
            .push_read_source(remote.clone()),
    );
    Layered {
        dir,
        primary,
        remote,
        repo,
    }
}

impl Layered {
    /// Run `op` and return the calls it made into the remote store.
    fn remote_calls(&self, op: impl FnOnce(&Repository)) -> Calls {
        let before = self.remote.snapshot();
        op(&self.repo);
        self.remote.snapshot().since(before)
    }

    /// Export both layers to disk and let the system `git` verify the history.
    fn assert_git_accepts(&self, commits: usize) {
        let root = self.dir.path();
        export_to_disk(root, &self.remote.inner);
        export_to_disk(root, &self.primary);
        git(root, &["fsck", "--full", "--strict"]);
        assert_eq!(
            git(root, &["rev-list", "--count", "HEAD"]).trim(),
            commits.to_string()
        );
    }
}

/// The cache-tree reuse check covers read sources too: cached subtrees the remote holds
/// are confirmed in one batch, so a one-file commit does not rebuild and re-probe every
/// unchanged directory through the remote, and neither does the next one.
#[test]
fn layered_one_file_commits_round_trips_do_not_grow_with_directory_count() {
    for dirs in [10, 1000] {
        let layered = layered_repo(dirs);
        let root = layered.dir.path().to_path_buf();
        for (n, contents) in ["changed\n", "changed again\n"].into_iter().enumerate() {
            fs::write(root.join("d0/f0"), contents).expect("modify");
            let calls = layered.remote_calls(|repo| commit_all(repo, "touch one file"));
            eprintln!("layered dirs={dirs} commit {n}: {calls:?}");
            assert!(
                calls.round_trips() <= 16,
                "dirs={dirs} commit {n}: {calls:?}"
            );
        }
        layered.assert_git_accepts(3);
        assert_eq!(git(&root, &["status", "--porcelain"]), "");
        assert_eq!(git(&root, &["show", "HEAD:d0/f0"]), "changed again\n");
    }
}

/// Stash push writes its index tree through the same cache-tree update, so stashing a
/// few edits on a layered store stays a bounded number of remote round trips.
#[test]
fn layered_stash_push_round_trips_do_not_grow_with_directory_count() {
    let dirs = 1000;
    let layered = layered_repo(dirs);
    let root = layered.dir.path().to_path_buf();
    for path in ["d0/f0", "d1/f1", "d2/f2"] {
        fs::write(root.join(path), "edited\n").expect("modify");
    }
    let calls = layered.remote_calls(|repo| {
        let options = StashPushOptions {
            message: Some("wip".to_owned()),
            include_untracked: false,
            identity: IDENT.parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        repo.stash()
            .push(
                &options,
                &mut NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("stash push")
            .expect("changes to stash");
    });
    eprintln!("layered stash push dirs={dirs}: {calls:?}");
    assert!(calls.round_trips() <= 16, "{calls:?}");
    layered.assert_git_accepts(1);
    assert_eq!(fs::read_to_string(root.join("d0/f0")).expect("read"), "0\n");
    assert_eq!(git(&root, &["show", "stash@{0}:d1/f1"]), "edited\n");
}

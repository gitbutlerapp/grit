//! On-demand promisor object fetch (partial clone lazy fetch).

#![cfg(feature = "http-ureq")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigFile;
use grit_lib::config::ConfigScope;
use grit_lib::diff::{
    build_file_diffs, diff_commits_with_renames_and_policy, DiffDetail, DiffOptions,
};
use grit_lib::error::Error;
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::DefaultHttpClientFactory;
use grit_lib::repo::Repository;
use grit_test_support::git_cmd;
use tempfile::TempDir;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn setup_blobless_upstream() -> (TempDir, PathBuf) {
    let layout = TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    for (name, body) in [
        ("a.txt", b"aaa\n"),
        ("b.txt", b"bbb\n"),
        ("c.txt", b"ccc\n"),
    ] {
        fs::write(upstream.join(name), body).expect("write");
    }
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "one", "-q"]);
    fs::write(upstream.join("d.txt"), b"ddd\n").expect("write");
    git_in(&upstream, &["add", "d.txt"]);
    git_in(&upstream, &["commit", "-m", "two", "-q"]);

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    (layout, bare)
}

fn grit_blobless_clone(
    url: &str,
    dest: &Path,
    fetcher: Option<Arc<dyn PromisorFetcher>>,
) -> Repository {
    fs::create_dir_all(dest).expect("dest");
    let outcome = clone(
        CloneOptions {
            url: url.to_owned(),
            dest: dest.to_path_buf(),
            filter: Some(ObjectFilterSpec::BlobNone),
            checkout: false,
            ..CloneOptions::new(url, dest.to_path_buf())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone");
    clear_pack_cache();
    let mut opts = RepositoryOptions::empty().with_work_tree(dest);
    if let Some(f) = fetcher {
        opts = opts.with_promisor_fetcher(Some(f));
    }
    Repository::open(outcome.repo.git_dir(), &opts).expect("reopen")
}

fn promisor_sidecar_count(objects: &Path) -> usize {
    fs::read_dir(objects.join("pack"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "promisor"))
        .count()
}

fn pack_file_count(objects: &Path) -> usize {
    fs::read_dir(objects.join("pack"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count()
}

fn setup_mass_add_delete_blobless_upstream(file_count: usize) -> (TempDir, PathBuf) {
    let layout = TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    for i in 1..=file_count {
        fs::write(
            upstream.join(format!("old-{i}.txt")),
            format!("old file {i}\n{}", "payload\n".repeat(20)),
        )
        .expect("write");
    }
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "base", "-q"]);
    for i in 1..=file_count {
        fs::remove_file(upstream.join(format!("old-{i}.txt"))).expect("rm");
        fs::write(
            upstream.join(format!("new-{i}.txt")),
            format!("new file {i}\n{}", "payload\n".repeat(20)),
        )
        .expect("write new");
    }
    git_in(&upstream, &["add", "-A"]);
    git_in(&upstream, &["commit", "-m", "renamed", "-q"]);

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    (layout, bare)
}

fn setup_many_file_blobless_upstream(file_count: usize) -> (TempDir, PathBuf) {
    let layout = TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    for i in 1..=file_count {
        fs::write(
            upstream.join(format!("file-{i}.txt")),
            format!("file {i}\n{}", "payload\n".repeat(20)),
        )
        .expect("write");
    }
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "base", "-q"]);
    for i in 1..=file_count {
        fs::write(
            upstream.join(format!("file-{i}.txt")),
            format!("file {i} v2\n{}", "payload\n".repeat(20)),
        )
        .expect("rewrite");
    }
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "changed", "-q"]);

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    (layout, bare)
}

fn tree_diff_entries_between_commits(
    repo: &Repository,
    old: &str,
    new: &str,
) -> Vec<grit_lib::diff::DiffEntry> {
    let old_commit = revision(repo, old);
    let new_commit = revision(repo, new);
    let old_tree = parse_commit(&repo.odb().read(&old_commit).expect("old commit").data)
        .expect("parse old")
        .tree;
    let new_tree = parse_commit(&repo.odb().read(&new_commit).expect("new commit").data)
        .expect("parse new")
        .tree;
    repo.diff()
        .tree_to_tree(Some(&old_tree), Some(&new_tree))
        .expect("tree diff")
}

struct CountingFetcher {
    calls: Arc<AtomicUsize>,
    inner: DefaultPromisorFetcher,
}

impl PromisorFetcher for CountingFetcher {
    fn fetch_missing(&self, repo: &Repository, oids: &[ObjectId]) -> grit_lib::error::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.fetch_missing(repo, oids)
    }
}

#[test]
fn lazy_read_fetches_missing_blob() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let blob_hex = git_cmd(&["rev-parse", "HEAD:a.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    assert!(!repo.odb().exists_local(&oid));

    let before_promisor_packs = promisor_sidecar_count(&repo.git_dir().join("objects"));
    let obj = repo.odb().read(&oid).expect("lazy read");
    assert_eq!(obj.data, b"aaa\n");
    assert!(
        promisor_sidecar_count(&repo.git_dir().join("objects")) > before_promisor_packs,
        "fetched pack should be promisor-marked"
    );
}

#[test]
fn no_fetcher_returns_typed_promised_missing() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let repo = grit_blobless_clone(&url, dest.path(), None);

    let blob_hex = git_cmd(&["rev-parse", "HEAD:b.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    let err = repo.odb().read(&oid).unwrap_err();
    assert!(
        matches!(err, Error::PromisedObjectMissing(_)),
        "expected PromisedObjectMissing, got {err:?}"
    );
}

#[test]
fn rename_detection_prefetch_batched_past_rename_limit() {
    let file_count = 600;
    let (_layout, bare) = setup_mass_add_delete_blobless_upstream(file_count);
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(CountingFetcher {
        calls: Arc::clone(&calls),
        inner: DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new())),
    });
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let objects = repo.git_dir().join("objects");
    let packs_before = pack_file_count(&objects);
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();
    let entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        &revision(&repo, "HEAD~1"),
        &revision(&repo, "HEAD"),
        50,
        config.as_ref(),
        &thread_policy,
    )
    .expect("commit diff with renames");
    assert_eq!(entries.len(), file_count);

    build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::Stat,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("stat");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "rename detection and stat must share one promisor fetch for {file_count} add/delete pairs"
    );
    assert_eq!(pack_file_count(&objects), packs_before + 1);
}

#[test]
fn tree_diff_prefetch_batched_past_rename_limit() {
    let file_count = 1200;
    let (_layout, bare) = setup_many_file_blobless_upstream(file_count);
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(CountingFetcher {
        calls: Arc::clone(&calls),
        inner: DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new())),
    });
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let objects = repo.git_dir().join("objects");
    let packs_before = pack_file_count(&objects);
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();
    let entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        &revision(&repo, "HEAD~1"),
        &revision(&repo, "HEAD"),
        50,
        config.as_ref(),
        &thread_policy,
    )
    .expect("commit diff with renames");
    assert_eq!(entries.len(), file_count);

    build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::Stat,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("stat");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "stat diff must batch {file_count} missing blobs into one promisor fetch"
    );
    assert_eq!(
        pack_file_count(&objects),
        packs_before + 1,
        "one promisor pack for the batch"
    );

    let sample = revision(&repo, "HEAD:file-1.txt");
    git_cmd(&["cat-file", "blob", &sample.to_hex()])
        .in_dir(&bare)
        .exec();
}

#[test]
fn tree_diff_prefetch_uses_single_request() {
    let file_count = 40;
    let (_layout, bare) = setup_many_file_blobless_upstream(file_count);
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(CountingFetcher {
        calls: Arc::clone(&calls),
        inner: DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new())),
    });
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let objects = repo.git_dir().join("objects");
    assert_eq!(promisor_sidecar_count(&objects), 1);
    let packs_before = pack_file_count(&objects);
    let entries = tree_diff_entries_between_commits(&repo, "HEAD~1", "HEAD");
    assert_eq!(entries.len(), file_count);
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();

    build_file_diffs(
        repo.odb(),
        entries.clone(),
        DiffDetail::NameOnly,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("name-only");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "name-only must not promisor-fetch blobs"
    );

    build_file_diffs(
        repo.odb(),
        entries.clone(),
        DiffDetail::Stat,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("stat");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "stat diff should batch missing blobs into one promisor fetch"
    );
    assert_eq!(pack_file_count(&objects), packs_before + 1);

    build_file_diffs(
        repo.odb(),
        entries,
        DiffDetail::Patch,
        DiffOptions::default(),
        config.as_ref(),
        &thread_policy,
    )
    .expect("patch");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "patch after stat must not refetch blobs already local"
    );
}

#[test]
fn checkout_prefetch_uses_single_request() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(CountingFetcher {
        calls: Arc::clone(&calls),
        inner: DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new())),
    });
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let head = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let head_commit = ObjectId::from_hex(head.trim()).expect("head");
    let empty = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty");
    let head_tree = parse_commit(&repo.odb().read(&head_commit).expect("head commit").data)
        .expect("parse head")
        .tree;

    checkout_between_trees(&repo, Some(&empty), &head_tree).expect("checkout");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "checkout should batch missing blobs into one promisor fetch"
    );
}

#[test]
fn lazy_fetch_opt_out_config() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let cfg_path = repo.git_dir().join("config");
    let content = fs::read_to_string(&cfg_path).expect("config");
    let mut cfg = ConfigFile::parse(&cfg_path, &content, ConfigScope::Local).expect("parse");
    cfg.set("fetch.promisorLazy", "false").expect("set");
    cfg.write().expect("write");
    repo.reload_config().expect("reload config");

    let blob_hex = git_cmd(&["rev-parse", "HEAD:c.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    let err = repo.odb().read(&oid).unwrap_err();
    assert!(
        matches!(err, Error::PromisedObjectMissing(_)),
        "opt-out must not fetch; got {err:?}"
    );
}

#[test]
fn blobless_clone_checkout_matches_git() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());

    let git_dest = TempDir::new().expect("git dest");
    git_in(
        git_dest.path().parent().unwrap(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-local",
            &url,
            git_dest.path().to_str().unwrap(),
        ],
    );

    let grit_dest = TempDir::new().expect("grit dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = grit_blobless_clone(&url, grit_dest.path(), Some(fetcher));
    let head = git_cmd(&["rev-parse", "HEAD"])
        .in_dir(grit_dest.path())
        .exec()
        .stdout;
    let head_commit = ObjectId::from_hex(head.trim()).expect("head");
    let head_tree = parse_commit(&repo.odb().read(&head_commit).expect("head commit").data)
        .expect("parse head")
        .tree;
    checkout_between_trees(&repo, None, &head_tree).expect("checkout");

    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        let git_bytes = fs::read(git_dest.path().join(name)).expect("git file");
        let grit_bytes = fs::read(grit_dest.path().join(name)).expect("grit file");
        assert_eq!(git_bytes, grit_bytes, "{name} content");
    }

    git_in(grit_dest.path(), &["status", "--porcelain"]);
    git_in(git_dest.path(), &["status", "--porcelain"]);
}

#[test]
fn lazy_read_into_and_read_info() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let blob_hex = git_cmd(&["rev-parse", "HEAD:b.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");

    let mut buf = Vec::new();
    let kind = repo.odb().read_into(&oid, &mut buf).expect("read_into");
    assert_eq!(kind, grit_lib::objects::ObjectKind::Blob);
    assert_eq!(buf, b"bbb\n");

    let info = repo.odb().read_info(&oid).expect("read_info");
    assert_eq!(info.kind, grit_lib::objects::ObjectKind::Blob);
    assert_eq!(info.size, 4);
}

struct NoOpFetcher;

impl PromisorFetcher for NoOpFetcher {
    fn fetch_missing(&self, _repo: &Repository, _oids: &[ObjectId]) -> grit_lib::error::Result<()> {
        Ok(())
    }
}

#[test]
fn lazy_fetch_no_materialization_errors() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let repo = grit_blobless_clone(&url, dest.path(), Some(Arc::new(NoOpFetcher)));

    let blob_hex = git_cmd(&["rev-parse", "HEAD:a.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    let err = repo.odb().read(&oid).unwrap_err();
    assert!(
        matches!(err, Error::Message(_)),
        "expected materialization failure, got {err:?}"
    );
}

#[test]
fn concurrent_lazy_reads_do_not_share_recursion_limit() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = Arc::new(grit_blobless_clone(&url, dest.path(), Some(fetcher)));

    let blobs = ["a.txt", "b.txt", "c.txt", "d.txt"];
    thread::scope(|scope| {
        for name in blobs {
            let repo = Arc::clone(&repo);
            scope.spawn(move || {
                let hex = git_cmd(&["rev-parse", &format!("HEAD:{name}")])
                    .in_dir(repo.work_tree().unwrap())
                    .exec()
                    .stdout;
                let oid = ObjectId::from_hex(hex.trim()).expect("oid");
                repo.odb().read(&oid).expect("concurrent lazy read");
            });
        }
    });
}

#[test]
fn repeated_checkout_does_not_refetch_promisor_blobs() {
    let (_layout, bare) = setup_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = Arc::new(CountingFetcher {
        calls: Arc::clone(&calls),
        inner: DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new())),
    });
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let head_commit = ObjectId::from_hex(
        git_cmd(&["rev-parse", "HEAD"])
            .in_dir(dest.path())
            .exec()
            .stdout
            .trim(),
    )
    .expect("head");
    let head_tree = parse_commit(&repo.odb().read(&head_commit).expect("commit").data)
        .expect("parse")
        .tree;
    let empty = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty");

    checkout_between_trees(&repo, Some(&empty), &head_tree).expect("first checkout");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    checkout_between_trees(&repo, Some(&empty), &head_tree).expect("second checkout");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second checkout must not refetch blobs already in promisor packs"
    );
}

fn setup_deltified_blobless_upstream() -> (TempDir, PathBuf) {
    let layout = TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    let base = vec![b'a'; 64 * 1024];
    fs::write(upstream.join("base.bin"), &base).expect("base");
    git_in(&upstream, &["add", "base.bin"]);
    git_in(&upstream, &["commit", "-m", "base", "-q"]);
    let mut delta = base.clone();
    delta.extend_from_slice(b"tail");
    fs::write(upstream.join("delta.bin"), &delta).expect("delta");
    git_in(&upstream, &["add", "delta.bin"]);
    git_in(&upstream, &["commit", "-m", "delta", "-q"]);

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    git_in(&bare, &["repack", "-adf"]);
    (layout, bare)
}

#[test]
fn noop_negotiation_fetches_deltified_missing_blob() {
    let (_layout, bare) = setup_deltified_blobless_upstream();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = TempDir::new().expect("dest");
    let fetcher = Arc::new(DefaultPromisorFetcher::new(Arc::new(
        DefaultHttpClientFactory::new(),
    )));
    let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher));

    let blob_hex = git_cmd(&["rev-parse", "HEAD:delta.bin"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let oid = ObjectId::from_hex(blob_hex.trim()).expect("oid");
    assert!(!repo.odb().exists_local(&oid));

    let obj = repo.odb().read(&oid).expect("read deltified blob");
    let expected = Command::new("git")
        .current_dir(&bare)
        .args(["cat-file", "blob", "HEAD:delta.bin"])
        .output()
        .expect("git cat-file");
    assert!(expected.status.success());
    assert_eq!(obj.data, expected.stdout);
}

// The materialization and sparse-scope scenarios are based on upstream Git's
// t5616 and t1090 behavior: metadata clones fetch only the blobs needed on disk.
fn revision(repo: &Repository, rev: &str) -> ObjectId {
    ObjectId::from_hex(
        git_cmd(&["rev-parse", rev])
            .in_dir(repo.work_tree().unwrap())
            .exec()
            .stdout
            .trim(),
    )
    .unwrap()
}

fn hard_reset(repo: &Repository, target: ObjectId) -> grit_lib::error::Result<()> {
    use grit_lib::ident::Signature;
    use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
    let identity = Signature::new(
        "T",
        "t@example.com",
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
    )
    .unwrap();
    reset_to_commit(
        repo,
        &ResetOptions::new(target, ResetMode::Hard, identity, "reset"),
        &mut grit_lib::progress::NullProgress,
        &NeverCancel,
    )
    .map(|_| ())
}

#[derive(Default)]
struct ResetFetcher {
    requests: std::sync::Mutex<Vec<Vec<ObjectId>>>,
}

impl PromisorFetcher for ResetFetcher {
    fn fetch_missing(&self, repo: &Repository, oids: &[ObjectId]) -> grit_lib::error::Result<()> {
        self.requests.lock().unwrap().push(oids.to_vec());
        DefaultPromisorFetcher::new(Arc::new(DefaultHttpClientFactory::new()))
            .fetch_missing(repo, oids)
    }
}

fn assert_clean_index_and_worktree(repo: &Repository, source: &Path, count: usize) {
    let index = repo.index_handle().load().unwrap();
    assert_eq!(index.entries().len(), count);
    for entry in index.entries() {
        assert_eq!(entry.stage(), 0);
        let path = std::str::from_utf8(&entry.path).unwrap();
        assert_eq!(
            fs::read(repo.work_tree().unwrap().join(path)).unwrap(),
            fs::read(source.join(path)).unwrap(),
            "{path}"
        );
        assert_eq!(entry.oid, revision(repo, &format!("HEAD:{path}")));
    }
    assert!(git_cmd(&["status", "--porcelain"])
        .in_dir(repo.work_tree().unwrap())
        .exec()
        .stdout
        .is_empty());
    git_in(repo.work_tree().unwrap(), &["fsck", "--strict"]);
}

#[test]
fn hard_reset_batches_200_missing_blobs_and_deduplicates() {
    let layout = TempDir::new().unwrap();
    git_in(layout.path(), &["init", "-q", "-b", "main", "source"]);
    let source = layout.path().join("source");
    for i in 1..=200 {
        fs::write(
            source.join(format!("file-{i}.txt")),
            format!("file {i}\n{}", "build payload\n".repeat(100)),
        )
        .unwrap();
    }
    fs::copy(source.join("file-1.txt"), source.join("duplicate.txt")).unwrap();
    git_in(&source, &["add", "."]);
    git_in(&source, &["commit", "-qm", "inputs"]);
    git_in(&source, &["config", "uploadpack.allowFilter", "true"]);
    let url = format!("file://{}", source.display());
    for existing_index in [false, true] {
        let dest = TempDir::new().unwrap();
        let fetcher = Arc::new(ResetFetcher::default());
        let repo = grit_blobless_clone(&url, dest.path(), Some(fetcher.clone()));
        let head = revision(&repo, "HEAD");
        assert_eq!(promisor_sidecar_count(&repo.git_dir().join("objects")), 1);
        assert!(!repo.odb().exists_local(&revision(&repo, "HEAD:file-1.txt")));
        if existing_index {
            git_in(dest.path(), &["read-tree", "HEAD"]);
            // A staged deletion must be repaired along with absent worktree paths.
            git_in(
                dest.path(),
                &["update-index", "--force-remove", "file-1.txt"],
            );
        }
        hard_reset(&repo, head).unwrap();
        let requests = fetcher.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "all reset repairs must share one fetch");
        assert_eq!(requests[0].len(), 200);
        assert_eq!(
            requests[0]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            200
        );
        drop(requests);
        assert_eq!(promisor_sidecar_count(&repo.git_dir().join("objects")), 2);
        assert_clean_index_and_worktree(&repo, &source, 201);
        hard_reset(&repo, head).unwrap();
        assert_eq!(
            fetcher.requests.lock().unwrap().len(),
            1,
            "second reset must use local blobs"
        );
    }
}

#[test]
fn hard_reset_batches_changed_and_unchanged_missing_paths_with_attributes() {
    let (_layout, bare) = setup_blobless_upstream();
    let source = bare.parent().unwrap().join("upstream");
    fs::write(source.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/.gitattributes"), "*.txt text eol=lf\n").unwrap();
    fs::write(source.join("nested/input.txt"), "nested\n").unwrap();
    git_in(&source, &["add", "."]);
    git_in(&source, &["commit", "-qm", "attributes"]);
    git_in(&source, &["branch", "target"]);
    fs::write(source.join("a.txt"), "new a\n").unwrap();
    fs::write(source.join("new.txt"), "only new\n").unwrap();
    git_in(&source, &["add", "."]);
    git_in(&source, &["commit", "-qm", "new tree"]);
    git_in(&source, &["push", bare.to_str().unwrap(), "main", "target"]);
    let dest = TempDir::new().unwrap();
    let fetcher = Arc::new(ResetFetcher::default());
    let repo = grit_blobless_clone(
        &format!("file://{}", bare.display()),
        dest.path(),
        Some(fetcher.clone()),
    );
    let target = revision(&repo, "HEAD~1");
    let unwanted = revision(&repo, "HEAD:new.txt");
    hard_reset(&repo, target).unwrap();
    assert_eq!(
        fetcher.requests.lock().unwrap().len(),
        1,
        "diff writes, attribute reads and unchanged repairs share a fetch"
    );
    assert!(
        !repo.odb().exists_local(&unwanted),
        "deleted paths need no content"
    );
    git_in(&source, &["reset", "--hard", "target"]);
    assert_clean_index_and_worktree(&repo, &source, 7);
    assert_eq!(promisor_sidecar_count(&repo.git_dir().join("objects")), 2);
}

#[test]
fn hard_reset_prefetch_respects_sparse_scope_and_gitlinks() {
    let (_layout, bare) = setup_blobless_upstream();
    let source = bare.parent().unwrap().join("upstream");
    fs::create_dir(source.join("outside")).unwrap();
    fs::write(source.join("outside/.gitattributes"), "*.txt text\n").unwrap();
    fs::write(source.join("outside/missing.txt"), "excluded\n").unwrap();
    git_in(&source, &["add", "."]);
    git_in(
        &source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "160000,{},submodule",
                git_cmd(&["rev-parse", "HEAD"])
                    .in_dir(&source)
                    .exec()
                    .stdout
                    .trim()
            ),
        ],
    );
    git_in(&source, &["commit", "-qm", "sparse and gitlink"]);
    git_in(&source, &["push", bare.to_str().unwrap(), "main"]);
    let dest = TempDir::new().unwrap();
    let fetcher = Arc::new(ResetFetcher::default());
    let repo = grit_blobless_clone(
        &format!("file://{}", bare.display()),
        dest.path(),
        Some(fetcher.clone()),
    );
    git_in(dest.path(), &["config", "core.sparseCheckout", "true"]);
    fs::create_dir_all(repo.git_dir().join("info")).unwrap();
    fs::write(
        repo.git_dir().join("info/sparse-checkout"),
        "/*\n!/outside/\n",
    )
    .unwrap();
    repo.reload_config().unwrap();
    hard_reset(&repo, revision(&repo, "HEAD")).unwrap();
    let requests = fetcher.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].len(), 4, "only included blobs, no gitlinks");
    for path in ["outside/.gitattributes", "outside/missing.txt"] {
        let oid = revision(&repo, &format!("HEAD:{path}"));
        assert!(!repo.odb().exists_local(&oid));
        assert!(!dest.path().join(path).exists());
        assert!(repo
            .index_handle()
            .load()
            .unwrap()
            .get(path.as_bytes(), 0)
            .unwrap()
            .skip_worktree());
    }
    assert!(
        git_cmd(&["status", "--porcelain", "--ignore-submodules=all"])
            .in_dir(dest.path())
            .exec()
            .stdout
            .is_empty()
    );
    git_in(dest.path(), &["fsck", "--strict"]);
}

#[test]
fn hard_reset_preserves_lazy_fetch_opt_outs_and_missing_fetcher_errors() {
    for disable in ["missing", "config", "env"] {
        let (_layout, bare) = setup_blobless_upstream();
        let dest = TempDir::new().unwrap();
        let fetcher = Arc::new(ResetFetcher::default());
        let repo = grit_blobless_clone(&format!("file://{}", bare.display()), dest.path(), None);
        if disable == "config" {
            git_in(dest.path(), &["config", "fetch.promisorLazy", "false"]);
        }
        let mut opts = RepositoryOptions::empty().with_work_tree(dest.path());
        if disable != "missing" {
            opts = opts.with_promisor_fetcher(Some(fetcher.clone()));
        }
        if disable == "env" {
            opts.environment = grit_lib::config::environment::Environment::from_vars(
                [("GRIT_NO_LAZY_FETCH".into(), "1".into())],
                dest.path().to_path_buf(),
            );
        }
        let repo = Repository::open(repo.git_dir(), &opts).unwrap();
        let err = hard_reset(&repo, revision(&repo, "HEAD")).unwrap_err();
        assert!(
            matches!(err, Error::PromisedObjectMissing(_)),
            "{disable}: {err:?}"
        );
        assert!(fetcher.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn hard_reset_keeps_stat_clean_files_without_fetching_their_missing_blobs() {
    let (_layout, bare) = setup_blobless_upstream();
    let source = bare.parent().unwrap().join("upstream");
    fs::write(source.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    git_in(&source, &["add", ".gitattributes"]);
    git_in(&source, &["commit", "-qm", "attributes"]);
    git_in(&source, &["push", bare.to_str().unwrap(), "main"]);
    let dest = TempDir::new().unwrap();
    let fetcher = Arc::new(ResetFetcher::default());
    let repo = grit_blobless_clone(
        &format!("file://{}", bare.display()),
        dest.path(),
        Some(fetcher.clone()),
    );
    let mut index = grit_lib::index::Index::new();
    for path in [".gitattributes", "a.txt", "b.txt", "c.txt", "d.txt"] {
        let abs = dest.path().join(path);
        fs::copy(source.join(path), &abs).unwrap();
        filetime::set_file_mtime(&abs, filetime::FileTime::from_unix_time(1_600_000_000, 0))
            .unwrap();
        let oid = revision(&repo, &format!("HEAD:{path}"));
        assert!(!repo.odb().exists_local(&oid));
        index.entries_mut().push(
            grit_lib::index::entry_from_stat(
                &abs,
                path.as_bytes(),
                oid,
                grit_lib::index::MODE_REGULAR,
            )
            .unwrap(),
        );
    }
    index.sort();
    repo.index_handle().write(&mut index).unwrap();
    hard_reset(&repo, revision(&repo, "HEAD")).unwrap();
    assert!(fetcher.requests.lock().unwrap().is_empty());
    assert!(!repo.odb().exists_local(&revision(&repo, "HEAD:a.txt")));
    assert_clean_index_and_worktree(&repo, &source, 5);
}

#[test]
fn hard_reset_propagates_typed_prefetch_failure() {
    struct CancelledFetcher;
    impl PromisorFetcher for CancelledFetcher {
        fn fetch_missing(&self, _: &Repository, _: &[ObjectId]) -> grit_lib::error::Result<()> {
            Err(Error::Cancelled)
        }
    }
    let (_layout, bare) = setup_blobless_upstream();
    let dest = TempDir::new().unwrap();
    let repo = grit_blobless_clone(
        &format!("file://{}", bare.display()),
        dest.path(),
        Some(Arc::new(CancelledFetcher)),
    );
    let head = revision(&repo, "HEAD");
    assert!(matches!(hard_reset(&repo, head), Err(Error::Cancelled)));
    assert_eq!(revision(&repo, "HEAD"), head);
}

#[test]
fn hard_reset_with_every_path_sparse_skipped_does_not_fetch_attributes() {
    let (_layout, bare) = setup_blobless_upstream();
    let source = bare.parent().unwrap().join("upstream");
    fs::write(source.join(".gitattributes"), "*.txt text\n").unwrap();
    git_in(&source, &["add", ".gitattributes"]);
    git_in(&source, &["commit", "-qm", "attributes"]);
    git_in(&source, &["push", bare.to_str().unwrap(), "main"]);
    let dest = TempDir::new().unwrap();
    let fetcher = Arc::new(ResetFetcher::default());
    let repo = grit_blobless_clone(
        &format!("file://{}", bare.display()),
        dest.path(),
        Some(fetcher.clone()),
    );
    git_in(dest.path(), &["config", "core.sparseCheckout", "true"]);
    fs::create_dir_all(repo.git_dir().join("info")).unwrap();
    fs::write(repo.git_dir().join("info/sparse-checkout"), "!/*\n").unwrap();
    repo.reload_config().unwrap();
    hard_reset(&repo, revision(&repo, "HEAD")).unwrap();
    assert!(fetcher.requests.lock().unwrap().is_empty());
    assert_eq!(promisor_sidecar_count(&repo.git_dir().join("objects")), 1);
    let index = repo.index_handle().load().unwrap();
    assert_eq!(index.entries().len(), 5);
    assert!(index.entries().iter().all(|entry| entry.skip_worktree()));
    for entry in index.entries() {
        assert!(!repo.odb().exists_local(&entry.oid));
        assert!(!dest
            .path()
            .join(std::str::from_utf8(&entry.path).unwrap())
            .exists());
    }
    git_in(dest.path(), &["fsck", "--strict"]);
}

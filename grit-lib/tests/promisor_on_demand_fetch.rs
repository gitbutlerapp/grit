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
use grit_lib::error::Error;
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::{parse_commit, ObjectId};
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

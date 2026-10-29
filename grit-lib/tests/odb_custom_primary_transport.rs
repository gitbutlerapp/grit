//! Transport, fsck, unbundle, and staging behavior for repositories whose object database
//! uses a custom [`OdbBuilder::primary`] store (issues #1735, #1892, #2288).
//!
//! Fetch and push read and write through the custom primary. Unbundling writes every object
//! of the bundle's pack into the custom primary. Staging writes go through the custom primary,
//! and fsck validates the objects the primary lists.

use grit_test_support::{git, rev_parse};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::bundle::BundleError;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::stage_tracked::IndexStagingThresholds;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, UpdateMode};
use grit_lib::remote::{Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

/// Non-bare upstream with two commits on `main`, written by the system `git`.
fn git_upstream() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    fs::write(dir.path().join("b.txt"), "two\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c2"]);
    dir
}

fn bare_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "--bare", "-b", "main", "."]);
    dir
}

fn init_files_repo(dir: &Path) {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
}

fn memory_repo() -> (tempfile::TempDir, Repository, Arc<MemoryStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    init_files_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(dir.path()))
        .expect("open");
    (dir, repo, store)
}

/// Every file under `objects/` (relative paths, sorted).
fn object_files(git_dir: &Path) -> Vec<String> {
    let base = git_dir.join("objects");
    let mut out = Vec::new();
    let mut stack = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in read.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(&base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

fn fetch_all_branches() -> FetchOptions {
    FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
        ..Default::default()
    }
}

/// Commit `blob` as `f.txt` on `refs/heads/main` through the repository's ODB.
fn commit_into(repo: &Repository, content: &[u8]) -> ObjectId {
    let blob = repo.odb().write(ObjectKind::Blob, content).expect("blob");
    let tree = repo
        .odb()
        .write(
            ObjectKind::Tree,
            &grit_lib::objects::serialize_tree(&[grit_lib::objects::TreeEntry {
                mode: 0o100644,
                name: b"f.txt".to_vec(),
                oid: blob,
            }]),
        )
        .expect("tree");
    let body = format!(
        "tree {tree}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n"
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit");
    grit_lib::refs::update_direct_ref_on_store(
        repo.refs(),
        repo.git_dir(),
        "refs/heads/main",
        &commit,
    )
    .expect("ref");
    commit
}

fn count_store_objects(store: &MemoryStore) -> usize {
    let mut n = 0usize;
    store
        .for_each_object(&mut |_| {
            n += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("enumerate");
    n
}

#[test]
fn fetch_into_custom_primary_round_trips_with_git() {
    let upstream = git_upstream();
    let (_dir, repo, store) = memory_repo();
    let before = object_files(repo.git_dir());

    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    repo.remotes()
        .fetch(
            &remote,
            fetch_all_branches(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch into custom primary");

    let tip = resolve_ref(repo.git_dir(), "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(tip, rev_parse(upstream.path(), "main"));
    assert!(store.contains(&tip).expect("contains"));
    assert!(repo.odb().read(&tip).is_ok());
    assert!(count_store_objects(&store) > 0);
    assert_eq!(
        object_files(repo.git_dir()),
        before,
        "fetch must not leave pack files under objects/ for a custom primary"
    );
    git(upstream.path(), &["fsck", "--full"]);
}

#[test]
fn fetch_from_bundle_into_custom_primary_succeeds() {
    let upstream = git_upstream();
    let bundle = upstream.path().join("all.bundle");
    git(
        upstream.path(),
        &["bundle", "create", bundle.to_str().expect("utf8"), "main"],
    );
    let (_dir, repo, store) = memory_repo();
    let before = object_files(repo.git_dir());

    let remote = Remote::from_url(bundle.to_str().expect("utf8")).expect("remote");
    repo.remotes()
        .fetch(
            &remote,
            fetch_all_branches(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("bundle fetch into custom primary");
    let tip = resolve_ref(repo.git_dir(), "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(tip, rev_parse(upstream.path(), "main"));
    assert!(store.contains(&tip).expect("contains"));
    assert_eq!(object_files(repo.git_dir()), before);
}

#[test]
fn fetch_incremental_into_custom_primary_uses_haves_from_store() {
    let upstream = git_upstream();
    let (_dir, repo, store) = memory_repo();
    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    repo.remotes()
        .fetch(
            &remote,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".into()],
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("initial fetch");
    let after_first = count_store_objects(&store);

    fs::write(upstream.path().join("c.txt"), "three\n").expect("write");
    git(upstream.path(), &["add", "c.txt"]);
    git(upstream.path(), &["commit", "-q", "-m", "c3"]);

    repo.remotes()
        .fetch(
            &remote,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".into()],
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("incremental fetch");
    assert!(count_store_objects(&store) > after_first);
    let tip = resolve_ref(repo.git_dir(), "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(tip, rev_parse(upstream.path(), "main"));
}

#[test]
fn fetch_dry_run_into_custom_primary_classifies_without_persisting() {
    let upstream = git_upstream();
    let (_dir, repo, store) = memory_repo();
    let before_count = count_store_objects(&store);

    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    let outcome = repo
        .remotes()
        .fetch(
            &remote,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".into()],
                dry_run: true,
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("dry-run fetch");
    assert!(outcome.updates.iter().any(|u| u.mode == UpdateMode::New));
    assert_eq!(count_store_objects(&store), before_count);
    assert!(resolve_ref(repo.git_dir(), "refs/remotes/origin/main").is_err());
}

#[test]
fn unbundle_into_custom_primary_writes_every_object_into_the_store() {
    let upstream = git_upstream();
    let bundle = upstream.path().join("all.bundle");
    git(
        upstream.path(),
        &["bundle", "create", bundle.to_str().expect("utf8"), "main"],
    );
    let (_dir, repo, store) = memory_repo();
    let before = object_files(repo.git_dir());

    let outcome = repo.bundles().unbundle(&bundle).expect("unbundle");
    let tip = rev_parse(upstream.path(), "main");
    assert_eq!(outcome.refs.len(), 1);
    assert_eq!(outcome.refs[0].oid, tip);

    let expected: Vec<ObjectId> = git(upstream.path(), &["rev-list", "--objects", "main"])
        .lines()
        .map(|line| ObjectId::from_hex(&line[..40]).expect("oid"))
        .collect();
    assert_eq!(expected.len(), 6, "two commits, two trees, two blobs");
    assert_eq!(outcome.transfer.objects, 6);
    assert_eq!(
        outcome.transfer.bytes,
        fs::metadata(&bundle).expect("meta").len()
            - grit_lib::bundle::Bundle::open(&bundle)
                .expect("open")
                .pack_byte_offset()
    );
    assert_eq!(
        outcome.pack_object_ids,
        expected
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
    );
    for oid in &expected {
        let obj = store.read(oid).expect("read").expect("object in store");
        let rehashed = grit_lib::objects::hash::hash_object(HashAlgo::Sha1, obj.kind, &obj.data)
            .expect("hash");
        assert_eq!(&rehashed, oid);
    }
    assert_eq!(
        object_files(repo.git_dir()),
        before,
        "unbundle must not stage the pack under objects/ for a custom primary"
    );

    // An incremental bundle whose prerequisite is already in the store.
    fs::write(upstream.path().join("c.txt"), "three\n").expect("write");
    git(upstream.path(), &["add", "c.txt"]);
    git(upstream.path(), &["commit", "-q", "-m", "c3"]);
    let incremental = upstream.path().join("incr.bundle");
    git(
        upstream.path(),
        &[
            "bundle",
            "create",
            incremental.to_str().expect("utf8"),
            "main~1..main",
        ],
    );
    let opened = grit_lib::bundle::Bundle::open(&incremental).expect("open bundle");
    assert!(opened
        .verify(&repo)
        .expect("verify")
        .missing_prerequisites
        .is_empty());
    let new_tip = rev_parse(upstream.path(), "main");
    let outcome = repo
        .bundles()
        .unbundle(&incremental)
        .expect("unbundle incremental");
    assert_eq!(outcome.refs[0].oid, new_tip);
    assert!(store.contains(&new_tip).expect("contains"));
    assert!(repo.odb().read(&new_tip).is_ok());
}

#[test]
fn unbundle_into_custom_primary_reports_a_corrupt_pack() {
    let upstream = git_upstream();
    let bundle = upstream.path().join("all.bundle");
    git(
        upstream.path(),
        &["bundle", "create", bundle.to_str().expect("utf8"), "main"],
    );
    let mut bytes = fs::read(&bundle).expect("read bundle");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&bundle, &bytes).expect("corrupt trailer");
    let (_dir, repo, _store) = memory_repo();

    let err = repo
        .bundles()
        .unbundle(&bundle)
        .expect_err("corrupt trailer must fail");
    assert!(
        matches!(&err, BundleError::Io(msg) if msg.contains("checksum")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn fetch_all_succeeds_with_custom_primary() {
    let upstream = git_upstream();
    let (_dir, repo, _store) = memory_repo();
    repo.remotes()
        .add("origin", upstream.path().to_str().expect("utf8"))
        .expect("add remote");
    let config = repo.config().expect("config");

    let results = repo
        .remotes()
        .fetch_all(
            &config,
            &FetchOptions::default(),
            None,
            None,
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch_all");
    assert_eq!(results.len(), 1);
    let (name, outcome) = &results[0];
    assert_eq!(name, "origin");
    outcome.as_ref().expect("custom primary fetch");
    assert!(resolve_ref(repo.git_dir(), "refs/remotes/origin/main").is_ok());
}

#[test]
fn push_from_custom_primary_round_trips_with_git() {
    let bare = bare_git_repo();
    let (_dir, repo, store) = memory_repo();
    let tip = commit_into(&repo, b"only in memory\n");
    assert!(store.contains(&tip).expect("contains"));

    let remote = Remote::from_url(bare.path().to_str().expect("utf8")).expect("remote");
    let spec = PushRefSpec {
        src: Some(tip),
        src_ref: Some("refs/heads/main".into()),
        dst: "refs/heads/main".into(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        hook_src_ref: None,
    };
    repo.remotes()
        .push(
            &remote,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push from custom primary");
    assert_eq!(rev_parse(bare.path(), "refs/heads/main"), tip);
    git(bare.path(), &["fsck", "--full"]);
    git(bare.path(), &["cat-file", "-p", &tip.to_hex()]);
}

#[test]
fn partial_fetch_custom_primary_is_refused() {
    let upstream = git_upstream();
    let (_dir, repo, _store) = memory_repo();
    repo.remotes()
        .add("origin", upstream.path().to_str().expect("utf8"))
        .expect("add");
    let remote = repo.remotes().get("origin").expect("get");
    let err = repo
        .remotes()
        .fetch(
            &remote,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".into()],
                filter: Some(grit_lib::remote::partial_clone::ObjectFilterSpec::BlobNone),
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("partial fetch");
    assert!(
        matches!(
            err,
            RemoteError::Library(Error::UnsupportedObjectStore { operation }) if operation == "partial fetch"
        ),
        "{err:?}"
    );
}

#[test]
fn fsck_on_custom_primary_validates_the_store() {
    let (_dir, repo, _store) = memory_repo();
    commit_into(&repo, b"x\n");
    let options = FsckOptions::default();
    let report = repo
        .fsck(&options, &mut NullProgress, &NeverCancel)
        .expect("fsck reads objects through the custom primary");
    assert!(report.objects_checked > 0);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert!(report.ok(&repo.config().expect("config"), &options));
}

#[test]
fn stage_and_auto_stage_commit_write_through_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let before = object_files(repo.git_dir());
    fs::write(dir.path().join("new.txt"), "fresh\n").expect("write");
    fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
    fs::write(dir.path().join("sub/deep.txt"), "deep\n").expect("write");

    let outcome = stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage into custom primary");
    assert_eq!(outcome.added.len(), 2, "{outcome:?}");
    for content in [&b"fresh\n"[..], b"deep\n"] {
        let oid = grit_lib::objects::hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, content)
            .expect("hash");
        assert!(
            store.contains(&oid).expect("contains"),
            "blob {oid} missing"
        );
    }

    let req = CommitRequest {
        message: "first".into(),
        author: Some("T <t@example.com> 1 +0000".parse().expect("signature")),
        committer: Some("T <t@example.com> 1 +0000".parse().expect("signature")),
        ..Default::default()
    };
    let first = commit_with_scope(
        &repo,
        &req,
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit staged");

    // Force the parallel tracked-restage batch path for one modification.
    fs::write(dir.path().join("sub/deep.txt"), "deeper\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            mode: StageMode::Update,
            staging_thresholds: Some(IndexStagingThresholds {
                large_index_restage: 0,
                parallel_tracked_restage: 0,
            }),
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("parallel restage into custom primary");
    let deeper =
        grit_lib::objects::hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"deeper\n")
            .expect("hash");
    assert!(store.contains(&deeper).expect("contains"));

    // The auto-staging commit restages the other modification on the default path.
    fs::write(dir.path().join("new.txt"), "changed\n").expect("write");
    let req = CommitRequest {
        message: "second".into(),
        ..req
    };
    let second = commit_with_scope(
        &repo,
        &req,
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("auto-stage commit into custom primary");
    assert_ne!(first.oid, second.oid);
    let changed =
        grit_lib::objects::hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"changed\n")
            .expect("hash");
    assert!(store.contains(&changed).expect("contains"));
    assert!(repo.odb().read(&second.oid).is_ok());

    assert_eq!(
        object_files(repo.git_dir()),
        before,
        "staging must not write loose objects beside a custom primary"
    );
}

#[test]
fn files_primary_fetch_and_push_still_round_trip_with_git() {
    let upstream = git_upstream();
    let dir = tempfile::tempdir().expect("tempdir");
    init_files_repo(dir.path());
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    repo.remotes()
        .fetch(
            &remote,
            fetch_all_branches(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("files-primary fetch");
    let tip = resolve_ref(repo.git_dir(), "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(tip, rev_parse(upstream.path(), "main"));
    assert!(repo.odb().read(&tip).is_ok());
    git(dir.path(), &["fsck", "--full"]);

    let bare = bare_git_repo();
    let dest = Remote::from_url(bare.path().to_str().expect("utf8")).expect("remote");
    let spec = PushRefSpec {
        src: Some(tip),
        src_ref: None,
        dst: "refs/heads/main".into(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        hook_src_ref: None,
    };
    repo.remotes()
        .push(
            &dest,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("files-primary push");
    assert_eq!(rev_parse(bare.path(), "refs/heads/main"), tip);
    git(bare.path(), &["fsck", "--full"]);
}

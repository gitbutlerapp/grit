//! Transport, fsck, unbundle, and staging behavior for repositories whose object database
//! uses a custom [`OdbBuilder::primary`] store (issue #1735).
//!
//! Fetch, push, fsck, and unbundle still assume the files-backed `objects/` directory, so they
//! must fail up front with a typed [`Error::UnsupportedObjectStore`] instead of writing packs
//! the repository cannot read or moving refs to objects it does not have. Staging writes go
//! through the custom primary.

use std::fs;
use std::path::Path;
use std::process::Command;
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
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::{Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

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
    write_ref(repo.git_dir(), "refs/heads/main", &commit).expect("ref");
    commit
}

fn is_unsupported(err: &RemoteError, op: &str) -> bool {
    matches!(
        err,
        RemoteError::Library(Error::UnsupportedObjectStore { operation }) if *operation == op
    )
}

#[test]
fn fetch_into_custom_primary_fails_without_touching_refs_or_objects() {
    let upstream = git_upstream();
    let (_dir, repo, store) = memory_repo();
    let before = object_files(repo.git_dir());

    let remote = Remote::from_url(upstream.path().to_str().expect("utf8")).expect("remote");
    let err = repo
        .remotes()
        .fetch(&remote, fetch_all_branches(), &mut NoProgress, None)
        .expect_err("fetch into a custom primary must not report success");
    assert!(is_unsupported(&err, "fetch"), "unexpected error: {err:?}");

    assert!(
        resolve_ref(repo.git_dir(), "refs/remotes/origin/main").is_err(),
        "no tracking ref may point at objects the repository cannot read"
    );
    assert_eq!(
        object_files(repo.git_dir()),
        before,
        "fetch must not install packs or loose objects under objects/"
    );
    let mut stored = 0usize;
    store
        .for_each_object(&mut |_| {
            stored += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("enumerate");
    assert_eq!(
        stored, 0,
        "fetch must not leave objects in the custom store"
    );
}

#[test]
fn fetch_from_bundle_into_custom_primary_is_rejected() {
    let upstream = git_upstream();
    let bundle = upstream.path().join("all.bundle");
    git(
        upstream.path(),
        &["bundle", "create", bundle.to_str().expect("utf8"), "main"],
    );
    let (_dir, repo, _store) = memory_repo();
    let before = object_files(repo.git_dir());

    let remote = Remote::from_url(bundle.to_str().expect("utf8")).expect("remote");
    let err = repo
        .remotes()
        .fetch(&remote, fetch_all_branches(), &mut NoProgress, None)
        .expect_err("bundle fetch into a custom primary must fail");
    assert!(is_unsupported(&err, "fetch"), "unexpected error: {err:?}");
    assert!(resolve_ref(repo.git_dir(), "refs/remotes/origin/main").is_err());
    assert_eq!(object_files(repo.git_dir()), before);

    let err = repo
        .bundles()
        .unbundle(&bundle)
        .expect_err("unbundle into a custom primary must fail");
    assert_eq!(
        err,
        BundleError::UnsupportedObjectStore {
            operation: "unbundle"
        }
    );
    assert_eq!(object_files(repo.git_dir()), before);
}

#[test]
fn fetch_all_reports_typed_error_per_remote() {
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
            None,
        )
        .expect("fetch_all");
    assert_eq!(results.len(), 1);
    let (name, outcome) = &results[0];
    assert_eq!(name, "origin");
    let err = outcome
        .as_ref()
        .expect_err("custom primary fetch must fail");
    assert!(is_unsupported(err, "fetch"), "unexpected error: {err:?}");
    assert!(resolve_ref(repo.git_dir(), "refs/remotes/origin/main").is_err());
}

#[test]
fn push_from_custom_primary_fails_without_touching_remote() {
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
    };
    let err = repo
        .remotes()
        .push(
            &remote,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            None,
        )
        .expect_err("push from a custom primary must fail");
    assert!(is_unsupported(&err, "push"), "unexpected error: {err:?}");
    assert!(resolve_ref(bare.path(), "refs/heads/main").is_err());
    git(bare.path(), &["fsck", "--full"]);
}

#[test]
fn fsck_on_custom_primary_is_rejected_by_name() {
    let (_dir, repo, _store) = memory_repo();
    commit_into(&repo, b"x\n");
    let err = repo
        .fsck(&FsckOptions::default(), &mut NullProgress, &NeverCancel)
        .expect_err("fsck needs the files-backed object store");
    assert!(
        matches!(err, Error::UnsupportedObjectStore { operation: "fsck" }),
        "unexpected error: {err:?}"
    );
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
    let first = commit_with_scope(&repo, &req, CommitScope::Staged, &mut NullProgress)
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
    let second = commit_with_scope(&repo, &req, CommitScope::AutoStage, &mut NullProgress)
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
        .fetch(&remote, fetch_all_branches(), &mut NoProgress, None)
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
    };
    repo.remotes()
        .push(
            &dest,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            None,
        )
        .expect("files-primary push");
    assert_eq!(rev_parse(bare.path(), "refs/heads/main"), tip);
    git(bare.path(), &["fsck", "--full"]);
}

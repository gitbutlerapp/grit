//! Concurrent loose writes of the same object are idempotent (issue #1875).
//!
//! Several writers, each with its own [`Repository`] handle on one repository, publish the
//! same objects at the same time. Every write must return the object id, the published
//! file must read back byte-exact, no temporary files may be left in `objects/`, and the
//! system `git fsck --strict` must accept the result.

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    dir
}

fn open(work_tree: &Path) -> Repository {
    Repository::open(
        &work_tree.join(".git"),
        &RepositoryOptions::empty().with_work_tree(work_tree),
    )
    .expect("open repository")
}

/// Leftover `objects/xx/tmp_*` files.
fn tmp_files(work_tree: &Path) -> Vec<String> {
    let objects = work_tree.join(".git/objects");
    let mut out = Vec::new();
    for shard in std::fs::read_dir(objects).expect("objects dir").flatten() {
        if !shard.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        out.extend(
            files
                .flatten()
                .map(|f| f.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("tmp_")),
        );
    }
    out
}

/// Run `threads` writers, each on its own handle, that all write every blob in `blobs`
/// after a shared barrier; returns each thread's results in `blobs` order.
fn write_from_threads(
    work_tree: &Path,
    threads: usize,
    blobs: &Arc<Vec<Vec<u8>>>,
) -> Vec<Vec<grit_lib::error::Result<ObjectId>>> {
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let work_tree = work_tree.to_path_buf();
            let barrier = Arc::clone(&barrier);
            let blobs = Arc::clone(blobs);
            thread::spawn(move || {
                let repo = open(&work_tree);
                barrier.wait();
                blobs
                    .iter()
                    .map(|data| repo.odb().write(ObjectKind::Blob, data))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("writer thread"))
        .collect()
}

fn assert_all_writes_agree(
    work_tree: &Path,
    blobs: &[Vec<u8>],
    results: Vec<Vec<grit_lib::error::Result<ObjectId>>>,
) {
    let repo = open(work_tree);
    for (thread, per_thread) in results.into_iter().enumerate() {
        for (i, result) in per_thread.into_iter().enumerate() {
            let oid = match result {
                Ok(oid) => oid,
                Err(e) => panic!("thread {thread} write {i} failed: {e:?}"),
            };
            let expected = repo
                .odb()
                .hash_algo()
                .hash_object(ObjectKind::Blob, &blobs[i])
                .expect("hash");
            assert_eq!(oid, expected, "thread {thread} write {i}");
        }
    }
    for data in blobs {
        let oid = repo
            .odb()
            .hash_algo()
            .hash_object(ObjectKind::Blob, data)
            .expect("hash");
        let obj = repo.odb().read(&oid).expect("read back");
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert!(obj.data == *data, "object {oid} reads back byte-exact");
        let out = std::process::Command::new("git")
            .current_dir(work_tree)
            .args(["cat-file", "blob", &oid.to_hex()])
            .output()
            .expect("git cat-file");
        assert!(out.status.success(), "git cat-file {oid}");
        assert!(out.stdout == *data, "git reads {oid} byte-exact");
    }
    assert_eq!(tmp_files(work_tree), Vec::<String>::new(), "tmp files left");
    git(work_tree, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn identical_small_blobs_from_many_handles_all_succeed() {
    let dir = init_repo();
    let blobs: Arc<Vec<Vec<u8>>> =
        Arc::new((0..300).map(|j| format!("same{j}").into_bytes()).collect());
    let results = write_from_threads(dir.path(), 16, &blobs);
    assert_all_writes_agree(dir.path(), &blobs, results);
}

#[test]
fn identical_large_blobs_from_many_handles_all_succeed() {
    let dir = init_repo();
    let blobs: Arc<Vec<Vec<u8>>> = Arc::new(
        (0u32..6)
            .map(|j| {
                (0u32..2 * 1024 * 1024)
                    .map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(j) >> 11) as u8)
                    .collect()
            })
            .collect(),
    );
    let results = write_from_threads(dir.path(), 8, &blobs);
    assert_all_writes_agree(dir.path(), &blobs, results);
}

#[test]
fn distinct_blobs_from_one_shared_handle_all_succeed() {
    let dir = init_repo();
    let repo = Arc::new(open(dir.path()));
    let threads = 8;
    let per_thread = 100;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let repo = Arc::clone(&repo);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                (0..per_thread)
                    .map(|j| {
                        let data = format!("thread {t} blob {j}\n").into_bytes();
                        let oid = repo
                            .odb()
                            .write(ObjectKind::Blob, &data)
                            .unwrap_or_else(|e| panic!("thread {t} write {j}: {e:?}"));
                        (oid, data)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for handle in handles {
        for (oid, data) in handle.join().expect("writer thread") {
            assert!(repo.odb().read(&oid).expect("read").data == data);
        }
    }
    assert_eq!(
        tmp_files(dir.path()),
        Vec::<String>::new(),
        "tmp files left"
    );
    git(dir.path(), &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn write_over_an_object_git_already_published_succeeds() {
    let dir = init_repo();
    std::fs::write(dir.path().join("f"), b"from git\n").expect("write file");
    let git_oid = git(dir.path(), &["hash-object", "-w", "f"]);
    let repo = open(dir.path());
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, b"from git\n")
        .expect("write existing object");
    assert_eq!(oid.to_hex(), git_oid.trim());
    assert_eq!(tmp_files(dir.path()), Vec::<String>::new());
    git(dir.path(), &["fsck", "--strict"]);
}

//! Reflink checkout fills a worktree from a reference tree, and keeps a file only
//! when it hashes to the blob the tree names.
//!
//! The fixture names a blob that is deliberately **absent** from the object
//! database, so a checkout that succeeds can only have taken the file from the
//! reference tree; the control test proves the blob really is missing.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{serialize_tree, HashAlgo, ObjectId, ObjectKind, TreeEntry};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

/// Comfortably above the 4 KiB below which the checkout copies instead of cloning.
const PAYLOAD: usize = 16 * 1024;

/// `FICLONE` from `<linux/fs.h>`: `_IOW(0x94, 9, int)`.
const FICLONE: libc::c_ulong = 0x4004_9409;

/// A scratch tree on the filesystem holding the build, not `/tmp` (tmpfs answers
/// `EOPNOTSUPP`) so the tests measure what they think they measure.
fn scratch() -> TempDir {
    let root = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp"));
    fs::create_dir_all(&root).expect("scratch root");
    tempfile::Builder::new()
        .prefix("reflink-checkout-")
        .tempdir_in(&root)
        .expect("scratch dir")
}

fn payload(seed: u8) -> Vec<u8> {
    (0..PAYLOAD)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

/// Whether this filesystem clones at all; on one that does not, the fixtures
/// cannot distinguish a working reflink from a missing object.
fn supports_reflink(dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let source = dir.join("probe-source");
    let dest = dir.join("probe-dest");
    fs::write(&source, payload(1)).expect("probe source");
    let (Ok(src), Ok(out)) = (fs::File::open(&source), fs::File::create(&dest)) else {
        return false;
    };
    // SAFETY: both descriptors outlive the call and FICLONE only reads the source.
    (unsafe { libc::ioctl(out.as_raw_fd(), FICLONE, src.as_raw_fd()) }) == 0
}

fn ghost_blob_repo(dir: &Path) -> (Repository, ObjectId, Vec<u8>) {
    let repo = Repository::init(&dir.join("repo"), &InitOptions::default()).expect("init");
    let data = payload(5);
    let ghost = HashAlgo::Sha1
        .hash_object(ObjectKind::Blob, &data)
        .expect("hash blob");
    let tree = repo
        .odb()
        .write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o100644,
                name: b"big.bin".to_vec(),
                oid: ghost,
            }]),
        )
        .expect("tree object");
    assert!(!repo.odb().exists(&ghost), "the fixture blob stays absent");
    (repo, tree, data)
}

fn open(repo: &Repository, reference: Option<PathBuf>) -> Repository {
    let options = RepositoryOptions::empty().with_checkout_reference_tree(reference);
    Repository::discover(repo.work_tree(), &options).expect("open")
}

#[test]
fn a_matching_reference_tree_fills_the_worktree() {
    let dir = scratch();
    let (repo, tree, data) = ghost_blob_repo(dir.path());
    let reference = dir.path().join("reference");
    fs::create_dir_all(&reference).expect("reference dir");
    fs::write(reference.join("big.bin"), &data).expect("reference file");

    let checkout = open(&repo, Some(reference.clone()));
    assert_eq!(
        checkout.checkout_reference_tree(),
        Some(reference.as_path()),
        "the reference tree reaches the repository handle"
    );
    if let Err(err) = checkout_between_trees(&checkout, None, &tree) {
        if !supports_reflink(dir.path()) {
            eprintln!("SKIP: filesystem does not implement FICLONE ({err})");
            return;
        }
        panic!("checkout from the reference tree failed: {err}");
    }

    let written = fs::read(checkout.work_tree().unwrap().join("big.bin")).expect("worktree file");
    assert_eq!(written, data);
}

#[test]
fn a_stale_reference_file_is_never_kept() {
    let dir = scratch();
    let (repo, tree, wanted) = ghost_blob_repo(dir.path());
    let reference = dir.path().join("reference");
    fs::create_dir_all(&reference).expect("reference dir");
    let mut stale = payload(9);
    stale.truncate(wanted.len());
    assert_ne!(stale, wanted, "same size, different bytes");
    fs::write(reference.join("big.bin"), &stale).expect("stale reference file");

    let checkout = open(&repo, Some(reference));
    let result = checkout_between_trees(&checkout, None, &tree);

    assert!(
        result.is_err(),
        "a wrong reference file must not satisfy the checkout"
    );
    assert!(
        !checkout.work_tree().unwrap().join("big.bin").exists(),
        "the rejected clone leaves no file behind"
    );
}

#[test]
fn without_a_reference_tree_the_blob_is_required() {
    let dir = scratch();
    let (repo, tree, _data) = ghost_blob_repo(dir.path());

    let checkout = open(&repo, None);
    let result = checkout_between_trees(&checkout, None, &tree);

    assert!(
        result.is_err(),
        "the fixture blob is absent, so the ordinary checkout cannot succeed"
    );
}

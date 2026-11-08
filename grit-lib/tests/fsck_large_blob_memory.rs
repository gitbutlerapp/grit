//! Fsck must not load whole large blobs into memory (issue #2564).

#![cfg(all(target_os = "linux", feature = "full-tests"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::objects::{ObjectKind, ObjectId};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

const BLOB_BYTES: usize = 4 * 1024 * 1024;
const THRESHOLD: usize = 64 * 1024;
const MIB: u64 = 1024 * 1024;

fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("status");
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kib * 1024
}

fn init_with_threshold(dir: &TempDir) -> (Repository, ObjectId) {
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let config_path = dir.path().join(".git/config");
  let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let appended = format!("{existing}\n[core]\n\tbigFileThreshold = {THRESHOLD}\n");
    std::fs::write(&config_path, appended).expect("config");
    let blob = vec![0u8; BLOB_BYTES];
    let oid = repo.odb().write(ObjectKind::Blob, &blob).expect("write");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("reopen");
    assert!(
        repo.odb().big_file_threshold().expect("threshold") >= THRESHOLD as u64,
        "bigFileThreshold must apply"
    );
    (repo, oid)
}

#[test]
fn fsck_loose_large_blob_peak_memory_stays_bounded() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let (repo, _oid) = init_with_threshold(&dir);
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_rss_bytes();
    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let growth = peak_rss_bytes().saturating_sub(before);
    assert!(
        growth < BLOB_BYTES as u64 / 2,
        "loose fsck grew peak RSS by {} MiB for a {} MiB blob",
        growth / MIB,
        BLOB_BYTES as u64 / MIB
    );
}

#[test]
fn fsck_packed_large_blob_peak_memory_stays_bounded() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let (repo, oid) = init_with_threshold(&dir);
    // Repack into a single-object pack; remove loose copy.
    repo.maintenance()
        .repack(
            &grit_lib::maintenance::RepackOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("repack");
    let loose_path = repo.odb().object_path(&oid);
    if loose_path.exists() {
        std::fs::remove_file(loose_path).expect("remove loose");
    }
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_rss_bytes();
    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let growth = peak_rss_bytes().saturating_sub(before);
    assert!(
        growth < BLOB_BYTES as u64 / 2,
        "packed fsck grew peak RSS by {} MiB for a {} MiB blob",
        growth / MIB,
        BLOB_BYTES as u64 / MIB
    );
}

#[test]
fn fsck_streaming_loose_blob_hash_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let (repo, oid) = init_with_threshold(&dir);
    let path = repo.odb().object_path(&oid);
    let mut bytes = std::fs::read(&path).expect("read loose");
    if let Some(byte) = bytes.last_mut() {
        *byte ^= 0xff;
    }
    std::fs::write(&path, bytes).expect("corrupt");
    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    assert!(report.issues.iter().any(|i| matches!(
        i,
        grit_lib::maintenance::repository_fsck::FsckIssue::HashMismatch { .. }
    )));
}

#[test]
fn fsck_streaming_accepts_loose_blob_git_wrote() {
    let dir = TempDir::new().expect("tempdir");
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init")
        .success();
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["config", "core.bigFileThreshold", &THRESHOLD.to_string()])
        .status()
        .expect("config");
    let blob_path = dir.path().join("big.bin");
    std::fs::write(&blob_path, vec![0u8; BLOB_BYTES]).expect("blob file");
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["add", "big.bin"])
        .status()
        .expect("add");
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "big"])
        .status()
        .expect("commit");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
}

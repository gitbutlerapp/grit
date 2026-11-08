//! Fsck must not load whole large blobs into memory (issue #2564).

#![cfg(all(target_os = "linux", feature = "full-tests"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::objects::prune_packed::{prune_packed_objects, PrunePackedOptions};
use grit_lib::objects::ObjectKind;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::objects::{flip_byte_at, write_loose_object, HashAlgo as FixtureAlgo};
use tempfile::TempDir;

const BLOB_BYTES: usize = 8 * 1024 * 1024;
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

fn write_config_threshold(dir: &TempDir) {
    let config_path = dir.path().join(".git/config");
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let appended = format!("{existing}\n[core]\n\tbigFileThreshold = {THRESHOLD}\n");
    std::fs::write(&config_path, appended).expect("config");
}

fn open_repo(dir: &TempDir) -> Repository {
    Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn fsck_loose_large_blob_peak_memory_stays_bounded() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    write_config_threshold(&dir);
    let blob = vec![0u8; BLOB_BYTES];
    repo.odb().write(ObjectKind::Blob, &blob).expect("write");
    let repo = open_repo(&dir);
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
fn fsck_staged_packed_large_blob_index_check_peak_memory_stays_bounded() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skip: cannot reset peak RSS");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    write_config_threshold(&dir);
    let blob_path = dir.path().join("big.bin");
    std::fs::write(&blob_path, vec![0u8; BLOB_BYTES]).expect("blob");
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["add", "big.bin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add")
        .success();
    std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "big"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("commit")
        .success();
    let repo = open_repo(&dir);
    repo.maintenance()
        .repack(
            &grit_lib::maintenance::RepackOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("repack");
    prune_packed_objects(repo.odb().objects_dir(), PrunePackedOptions::default())
        .expect("prune packed loose");
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
        "indexed packed fsck grew peak RSS by {} MiB for a {} MiB blob",
        growth / MIB,
        BLOB_BYTES as u64 / MIB
    );
}

#[test]
fn fsck_streaming_loose_blob_hash_mismatch() {
    const SIZE: usize = THRESHOLD * 4;
    let dir = TempDir::new().expect("tempdir");
    let (_, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    write_config_threshold(&dir);
    let objects = dir.path().join(".git/objects");
    let oid_hex = write_loose_object(&objects, FixtureAlgo::Sha1, "blob", &vec![1u8; SIZE])
        .expect("write loose");
    let path = objects.join(&oid_hex[..2]).join(&oid_hex[2..]);
    flip_byte_at(&path, path.metadata().expect("meta").len() - 1).expect("corrupt");
    let repo = open_repo(&dir);
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
            | grit_lib::maintenance::repository_fsck::FsckIssue::CorruptLoose { .. }
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
    let repo = open_repo(&dir);
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

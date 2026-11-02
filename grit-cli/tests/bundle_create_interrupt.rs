//! Ctrl+C during `grit bundle create` exits 130 and removes the partial file (issue #2489).

#![cfg(unix)]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{sigint, spawn_grit};

const FILES: usize = 2500;
const ATTEMPTS: usize = 8;

fn seed_repo(repo: &Path) -> String {
    fs::create_dir_all(repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(repo).suc();
    for i in 0..FILES {
        fs::write(
            repo.join(format!("b{i}.txt")),
            format!("blob {i}\n").repeat(400),
        )
        .expect("write");
    }
    grit_cmd(&["commit", "-m", "many"]).in_dir(repo).suc();
    "HEAD".to_owned()
}

fn bundle_part_path(path: &Path) -> std::path::PathBuf {
    path.with_extension("bundle.part")
}

fn bundle_growing(path: &Path) -> bool {
    let part = bundle_part_path(path);
    part.metadata().map(|m| m.len() > 16_384).unwrap_or(false)
        || path.metadata().map(|m| m.len() > 16_384).unwrap_or(false)
}

#[test]
fn ctrl_c_during_bundle_create_exits_130_and_removes_destination() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    let branch = seed_repo(&repo);
    let bundle = base.path().join("out.bundle");

    let mut cancelled = false;
    for _ in 0..ATTEMPTS {
        let _ = fs::remove_file(&bundle);
        let _ = fs::remove_file(bundle_part_path(&bundle));
        let mut child = spawn_grit(
            &repo,
            &["bundle", "create", bundle.to_str().expect("utf8"), &branch],
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if child.try_wait().expect("poll").is_some() {
                break;
            }
            if bundle_growing(&bundle) {
                sigint(&child);
                break;
            }
            assert!(Instant::now() < deadline, "bundle never started growing");
            std::thread::sleep(Duration::from_micros(500));
        }
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(130) && stderr.contains("operation cancelled") {
            cancelled = true;
            assert!(
                !bundle.exists() && !bundle_part_path(&bundle).exists(),
                "partial bundle left"
            );
            break;
        }
        if out.status.success() {
            continue;
        }
    }
    assert!(cancelled, "never interrupted bundle create mid-write");
}

const SENTINEL: &[u8] = b"pre-existing bundle sentinel\n";

#[test]
fn ctrl_c_during_bundle_create_preserves_pre_existing_destination() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    let branch = seed_repo(&repo);
    let bundle = base.path().join("existing.bundle");
    fs::write(&bundle, SENTINEL).expect("seed destination");

    let mut cancelled = false;
    for _ in 0..ATTEMPTS {
        fs::write(&bundle, SENTINEL).expect("restore sentinel");
        let _ = fs::remove_file(bundle_part_path(&bundle));
        let mut child = spawn_grit(
            &repo,
            &["bundle", "create", bundle.to_str().expect("utf8"), &branch],
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if child.try_wait().expect("poll").is_some() {
                break;
            }
            if bundle_growing(&bundle) {
                sigint(&child);
                break;
            }
            assert!(Instant::now() < deadline, "bundle never started growing");
            std::thread::sleep(Duration::from_micros(500));
        }
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(130) && stderr.contains("operation cancelled") {
            cancelled = true;
            assert!(
                !bundle_part_path(&bundle).exists(),
                "partial bundle part left"
            );
            assert_eq!(fs::read(&bundle).expect("read sentinel"), SENTINEL);
            break;
        }
        if out.status.success() {
            continue;
        }
    }
    assert!(
        cancelled,
        "never interrupted bundle create over pre-existing destination"
    );
}

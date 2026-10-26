//! Peak RSS guards for large blobs (issue #2010): stat/add/cat/clone must not load whole files.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const BLOB_MIB: usize = 64;
const MAX_GROWTH_BYTES: u64 = 24 * 1024 * 1024;

fn grit_binary() -> PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    let release = target_dir.join("release/grit");
    assert!(release.is_file(), "build grit-cli release or set GRIT_BIN");
    release
}

fn grit_bench_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    target_dir.join("release/grit-bench")
}

fn measure_peak_rss(cwd: &Path, cmd: &str, grit_bench: &Path) -> u64 {
    let out = Command::new(grit_bench)
        .arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env("GRIT_BENCH_MEASURE_CMD", cmd)
        .output()
        .expect("measure-rss");
    assert!(
        out.status.success(),
        "measure-rss: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss bytes")
}

fn write_random_blob(path: &Path) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create"));
    let mut chunk = vec![0u8; 1024 * 1024];
    for (i, byte) in chunk.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
        if i % 37 == 0 {
            *byte = 0;
        }
    }
    for _ in 0..BLOB_MIB {
        file.write_all(&chunk).expect("write");
    }
    file.flush().expect("flush");
}

fn init_repo(root: &Path) {
    grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
    grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
    grit_test_support::git(root, &["config", "user.name", "Test"]);
}

#[test]
fn large_blob_commands_stay_within_memory_budget() {
    if std::env::var("GRIT_LARGE_BLOB_MEMORY").ok().as_deref() != Some("1") {
        eprintln!("skip large_blob_memory (set GRIT_LARGE_BLOB_MEMORY=1)");
        return;
    }

    let grit = grit_binary();
    let grit_bench = grit_bench_binary();
    assert!(grit_bench.is_file(), "build grit-utils release");

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_random_blob(&root.join("big.bin"));
    let grit_s = grit.display().to_string();

    Command::new(&grit)
        .current_dir(root)
        .args(["add", "big.bin"])
        .status()
        .expect("add");
    Command::new(&grit)
        .current_dir(root)
        .args(["commit", "-m", "big"])
        .status()
        .expect("commit");

    let touch = format!(
        "python3 -c \"import os,time; p='big.bin'; os.utime(p,(time.time()+5,time.time()+5))\""
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&touch)
        .current_dir(root)
        .status()
        .expect("touch");

    let status_rss = measure_peak_rss(root, &format!("{grit_s} status"), &grit_bench);
    assert!(
        status_rss <= MAX_GROWTH_BYTES,
        "status peak {status_rss} > {MAX_GROWTH_BYTES}"
    );

    let add_rss = measure_peak_rss(root, &format!("{grit_s} add big.bin"), &grit_bench);
    assert!(
        add_rss <= MAX_GROWTH_BYTES,
        "add peak {add_rss} > {MAX_GROWTH_BYTES}"
    );

    let cat_rss = measure_peak_rss(
        root,
        &format!("{grit_s} cat HEAD:big.bin > /dev/null"),
        &grit_bench,
    );
    assert!(
        cat_rss <= MAX_GROWTH_BYTES,
        "cat peak {cat_rss} > {MAX_GROWTH_BYTES}"
    );

    let restore_rss = measure_peak_rss(
        root,
        &format!("rm -f big.bin && {grit_s} restore big.bin"),
        &grit_bench,
    );
    assert!(
        restore_rss <= MAX_GROWTH_BYTES,
        "restore peak {restore_rss} > {MAX_GROWTH_BYTES}"
    );

    let clone_dest = tmp.path().join("clone");
    let clone_rss = measure_peak_rss(
        root,
        &format!("{grit_s} clone . {} > /dev/null", clone_dest.display()),
        &grit_bench,
    );
    assert!(
        clone_rss <= MAX_GROWTH_BYTES.saturating_add(BLOB_MIB as u64 * 1024 * 1024 / 4),
        "clone peak {clone_rss} too high"
    );
}

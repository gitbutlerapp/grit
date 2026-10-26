//! Local clone must hard-link large loose blobs instead of repacking them in memory.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

const BLOB_MIB: usize = 64;
const MAX_GROWTH_KIB: i64 = 16 * 1024;

fn peak_rss_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable memory sized for `rusage`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage failed");
    // SAFETY: getrusage succeeded and initialised the struct.
    i64::from(unsafe { usage.assume_init() }.ru_maxrss)
}

fn write_big_file(path: &Path) {
    let mut file = std::io::BufWriter::new(fs::File::create(path).expect("create"));
    let mut written = 0usize;
    let mut n = 0u64;
    while written < BLOB_MIB * 1024 * 1024 {
        let line = format!("{n}\n");
        file.write_all(line.as_bytes()).expect("write");
        written += line.len();
        n += 1;
    }
    file.flush().expect("flush");
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) {
    git_stdout(dir, args);
}

fn loose_object_path(objects: &Path, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    objects.join(&hex[..2]).join(&hex[2..])
}

#[test]
fn local_clone_links_large_loose_blob_in_bounded_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    fs::create_dir_all(&source).expect("mkdir");
    git(&source, &["init", "-q", "-b", "main"]);
    git(&source, &["config", "user.email", "t@example.com"]);
    git(&source, &["config", "user.name", "Test"]);
    write_big_file(&source.join("big.txt"));
    git(&source, &["add", "big.txt"]);
    git(&source, &["commit", "-qm", "big"]);

    let blob_oid =
        ObjectId::from_hex(&git_stdout(&source, &["rev-parse", "HEAD:big.txt"])).expect("oid");
    let src_loose = loose_object_path(&source.join(".git/objects"), &blob_oid);

    let dest = dir.path().join("clone");
    let before = peak_rss_kib();
    clone_repo(
        CloneOptions::new(source.to_string_lossy(), dest.clone()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone");
    let growth = peak_rss_kib() - before;
    assert!(
        growth < MAX_GROWTH_KIB,
        "local clone RSS grew by {growth} KiB for a {BLOB_MIB} MiB loose blob"
    );

    let dest_loose = loose_object_path(&dest.join(".git/objects"), &blob_oid);
    assert!(
        dest_loose.is_file(),
        "clone should install the blob as a loose object (hard link), not only in a pack"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&src_loose).expect("src").ino(),
            fs::metadata(&dest_loose).expect("dst").ino(),
            "cloned blob should share inode with source on one filesystem"
        );
    }
    git(&dest, &["fsck", "--no-progress"]);
    let status = git_stdout(&dest, &["status", "--porcelain"]);
    assert!(status.is_empty(), "git status after clone:\n{status}");
}

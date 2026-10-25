//! Restoring or switching to a large blob must not load it into memory: the process's peak
//! resident set may grow by only a small fraction of the blob's size. Lives in its own test
//! binary so no other test moves the high-water mark.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::repo::Repository;

const BLOB_MIB: usize = 64;
/// Allowed peak-RSS growth while materializing the blob.
const MAX_GROWTH_KIB: i64 = 16 * 1024;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn peak_rss_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable memory sized for `rusage`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage failed");
    // SAFETY: getrusage succeeded and initialised the struct.
    i64::from(unsafe { usage.assume_init() }.ru_maxrss)
}

/// Write `BLOB_MIB` MiB of distinct text lines without holding them in memory.
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

fn files_equal(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (mut fa, mut fb) = (fs::File::open(a).expect("a"), fs::File::open(b).expect("b"));
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let na = fa.read(&mut ba).expect("read a");
        let mut filled = 0;
        while filled < na {
            let nb = fb.read(&mut bb[filled..na]).expect("read b");
            if nb == 0 {
                return false;
            }
            filled += nb;
        }
        if ba[..na] != bb[..na] {
            return false;
        }
        if na == 0 {
            return fb.read(&mut bb).expect("read b") == 0;
        }
    }
}

fn assert_growth(before: i64, context: &str) {
    let growth = peak_rss_kib() - before;
    assert!(
        growth < MAX_GROWTH_KIB,
        "{context}: peak RSS grew by {growth} KiB for a {BLOB_MIB} MiB blob"
    );
}

fn materialize_without_loading(packed: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);
    write_big_file(&root.join("big.txt"));
    let reference = dir.path().join("reference.txt");
    fs::copy(root.join("big.txt"), &reference).expect("keep a reference copy");
    fs::write(root.join(".gitignore"), "reference.txt\n").expect("ignore");
    git(root, &["add", "big.txt", ".gitignore"]);
    git(root, &["commit", "-qm", "big"]);
    git(root, &["switch", "-qc", "without"]);
    git(root, &["rm", "-q", "big.txt"]);
    git(root, &["commit", "-qm", "rm"]);
    git(root, &["switch", "-q", "main"]);
    if packed {
        git(root, &["repack", "-adq"]);
    }
    let main = ObjectId::from_hex(git(root, &["rev-parse", "main"]).trim()).expect("main");
    let without = ObjectId::from_hex(git(root, &["rev-parse", "without"]).trim()).expect("without");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let label = if packed { "packed" } else { "loose" };

    fs::remove_file(root.join("big.txt")).expect("rm");
    let before = peak_rss_kib();
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["big.txt".into()],
            pathspec_sources: vec!["big.txt".into()],
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore");
    assert_growth(before, &format!("{label} restore"));
    assert!(files_equal(&root.join("big.txt"), &reference));

    let switch = |branch: &str, oid: ObjectId| {
        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: branch.into(),
                    oid,
                },
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("switch");
    };
    switch("without", without);
    let before = peak_rss_kib();
    switch("main", main);
    assert_growth(before, &format!("{label} switch"));
    assert!(files_equal(&root.join("big.txt"), &reference));

    let status = git(root, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "{label}: git status:\n{status}");
    git(root, &["fsck", "--no-progress"]);
}

#[test]
fn restore_and_switch_stream_a_large_blob_in_bounded_memory() {
    materialize_without_loading(false);
    materialize_without_loading(true);
}

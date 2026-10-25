//! Peak memory of repository fsck on a delta-heavy pack.
//!
//! Lives in its own test binary so no other test allocates while the peak is measured.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use tempfile::TempDir;

const VERSIONS: usize = 1600;
const LINES: usize = 1024;
const LINE_BYTES: usize = 64;
const MIB: u64 = 1024 * 1024;

/// Bare repository whose single pack holds `VERSIONS` revisions of a 64 KiB file, each
/// one line away from the previous, written by `git fast-import` as delta chains.
fn delta_heavy_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir.path())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    };
    assert!(git(&["init", "-q", "--bare"])
        .status()
        .expect("git init")
        .success());

    let mut lines: Vec<String> = (0..LINES)
        .map(|i| format!("{:<width$}\n", format!("line {i}"), width = LINE_BYTES - 1))
        .collect();
    let mut stream = Vec::new();
    for version in 0..VERSIONS {
        lines[(version * 7) % LINES] = format!(
            "{:<width$}\n",
            format!("changed in {version}"),
            width = LINE_BYTES - 1
        );
        let content = lines.concat();
        let message = format!("version {version}\n");
        write!(
            stream,
            "commit refs/heads/main\ncommitter T <t@example.com> {} +0000\ndata {}\n{message}",
            1_700_000_000 + version,
            message.len()
        )
        .expect("stream");
        write!(stream, "M 100644 inline file.txt\ndata {}\n", content.len()).expect("stream");
        stream.extend_from_slice(content.as_bytes());
        stream.push(b'\n');
    }
    let mut child = git(&["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&stream)
        .expect("feed fast-import");
    assert!(child.wait().expect("fast-import").success());
    dir
}

/// `VmHWM` (peak resident set) of this process in bytes.
fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kib * 1024
}

#[test]
#[cfg(target_os = "linux")]
fn fsck_peak_memory_does_not_follow_uncompressed_pack_size() {
    let dir = delta_heavy_repo();
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let uncompressed = (VERSIONS * LINES * LINE_BYTES) as u64;

    // Writing 5 resets VmHWM to the current resident set, so the peak below is fsck's own.
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skipping: cannot reset peak RSS through /proc/self/clear_refs");
        return;
    }
    let before = peak_rss();
    let report = fsck(
        &repo,
        &FsckOptions::full_repository(false),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    let growth = peak_rss().saturating_sub(before);

    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert_eq!(report.objects_checked, 3 * VERSIONS);
    assert!(
        growth < uncompressed / 2,
        "fsck peak grew by {} MiB for a pack of {} MiB uncompressed",
        growth / MIB,
        uncompressed / MIB
    );
}

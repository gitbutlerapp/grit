//! Peak memory of serial tree structured diff vs retaining every file hunk list.
//!
//! Isolated test binary so other tests do not inflate the measured peak.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use grit_lib::diff::{
    diff_trees, for_each_tree_diff_structured, tree_diff_structured_parallel, DiffOptions,
};
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};

const FILES: usize = 120;
const LINES_PER_FILE: usize = 400;
const LINE_WIDTH: usize = 48;
const MIB: u64 = 1024 * 1024;

fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kib * 1024
}

fn reset_peak_rss() {
    if std::fs::write("/proc/self/clear_refs", "5").is_err() {
        eprintln!("skipping: cannot reset peak RSS through /proc/self/clear_refs");
    }
}

fn line_content(file: usize, line: usize, version: u8) -> String {
    format!(
        "{:<width$}\n",
        format!("f{file} L{line} v{version}"),
        width = LINE_WIDTH - 1
    )
}

fn tree_with_version(repo: &Repository, version: u8) -> grit_lib::objects::ObjectId {
    let odb = repo.odb();
    let entries: Vec<TreeEntry> = (0..FILES)
        .map(|file| {
            let body: String = (0..LINES_PER_FILE)
                .map(|line| line_content(file, line, version))
                .collect();
            let oid = odb.write(ObjectKind::Blob, body.as_bytes()).expect("blob");
            TreeEntry {
                mode: 0o100644,
                name: format!("file-{file:03}.txt").into_bytes(),
                oid,
            }
        })
        .collect();
    odb.write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("tree")
}

#[test]
#[cfg(target_os = "linux")]
fn serial_structured_diff_peak_stays_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let old_tree = tree_with_version(&repo, 0);
    let new_tree = tree_with_version(&repo, 1);
    let entries = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "").expect("diff trees");
    assert_eq!(entries.len(), FILES);
    let config = repo.config().expect("config");
    let thread_policy = ThreadPolicy::default();
    let diff_options = DiffOptions::default();

    reset_peak_rss();
    let before_serial = peak_rss();
    let live = AtomicUsize::new(0);
    let max_live = AtomicUsize::new(0);
    for_each_tree_diff_structured(repo.odb(), &entries, diff_options.clone(), |row| {
        let n = live.fetch_add(1, Ordering::Relaxed) + 1;
        max_live.fetch_max(n, Ordering::Relaxed);
        assert!(!row.structured.hunks.is_empty() || row.binary);
        live.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    })
    .expect("serial structured diff");
    let serial_growth = peak_rss().saturating_sub(before_serial);

    reset_peak_rss();
    let before_parallel = peak_rss();
    let _all =
        tree_diff_structured_parallel(repo.odb(), entries, &config, &thread_policy, diff_options)
            .expect("parallel structured diff");
    let parallel_growth = peak_rss().saturating_sub(before_parallel);

    assert_eq!(
        max_live.load(Ordering::Relaxed),
        1,
        "callback should hold one row"
    );
    assert!(
        serial_growth < parallel_growth / 3,
        "serial peak grew by {} MiB vs parallel {} MiB",
        serial_growth / MIB,
        parallel_growth / MIB
    );
}

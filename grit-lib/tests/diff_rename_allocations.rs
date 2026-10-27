//! Large inexact rename matrices must reuse per-blob scoring data.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[allow(unused_imports)]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    detect_copies_with_policy, detect_renames_with_policy, diff_trees, DiffStatus,
};
use grit_lib::parallel::ThreadPolicy;
use support::{git_output, git_quiet, rev_parse_tree, DiffRepo};

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    // This test uses a serial policy. Ignore other test threads and tolerate
    // allocations during TLS teardown, when the counter is no longer available.
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

// SAFETY: Every operation delegates to System with the original pointer/layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The allocation and layout came from System.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller supplies the original allocation and valid new size.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller supplies a valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn inexact_document_renames_do_not_allocate_per_candidate_pair() {
    let dr = DiffRepo::init();
    let root = &dr.worktree;
    std::fs::create_dir(root.join("old")).unwrap();
    std::fs::create_dir(root.join("new")).unwrap();
    const FILES: usize = 128;
    for file in 0..FILES {
        let body: String = (0..100)
            .map(|line| format!("File {file:04} line {line:03}: documentation payload.\n"))
            .collect();
        std::fs::write(root.join(format!("old/item-{file:04}.txt")), body).unwrap();
    }
    git_quiet(root, &["add", "."]);
    git_quiet(root, &["commit", "-qm", "base"]);
    let old_tree = rev_parse_tree(root, "HEAD^{tree}");
    for file in 0..FILES {
        let old_path = root.join(format!("old/item-{file:04}.txt"));
        let body = std::fs::read_to_string(&old_path).unwrap();
        std::fs::write(
            root.join(format!("new/review-{file:04}.adoc")),
            body.replace("line 050", "line NEW"),
        )
        .unwrap();
        std::fs::remove_file(old_path).unwrap();
    }
    git_quiet(root, &["add", "-A"]);
    git_quiet(root, &["commit", "-qm", "review"]);
    let new_tree = rev_parse_tree(root, "HEAD^{tree}");
    let raw = diff_trees(dr.repo.odb(), Some(&old_tree), Some(&new_tree), "").unwrap();
    assert_eq!(raw.len(), FILES * 2);
    let sources: Vec<_> = raw
        .iter()
        .filter(|entry| entry.status == DiffStatus::Deleted)
        .map(|entry| {
            (
                entry.old_path.clone().unwrap(),
                entry.old_mode.clone(),
                entry.old_oid,
            )
        })
        .collect();
    let added: Vec<_> = raw
        .iter()
        .filter(|entry| entry.status == DiffStatus::Added)
        .cloned()
        .collect();
    let config = ConfigSet::new();
    let policy = ThreadPolicy::with_threads_override(Some(1));

    let before = ALLOCATIONS.with(Cell::get);
    let renamed =
        detect_renames_with_policy(dr.repo.odb(), None, raw, 50, &config, &policy, None).unwrap();
    let allocations = ALLOCATIONS.with(Cell::get) - before;
    eprintln!("{FILES} inexact renames: {allocations} allocations");

    let expected = git_output(root, &["diff", "-M", "--name-status", "HEAD~1", "HEAD"]);
    let mut expected_paths: Vec<_> = String::from_utf8(expected.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split_once('\t').unwrap().1.to_owned())
        .collect();
    let mut actual_paths: Vec<_> = renamed
        .iter()
        .map(|entry| {
            assert_eq!(entry.status, DiffStatus::Renamed);
            format!(
                "{}\t{}",
                entry.old_path.as_deref().unwrap(),
                entry.new_path.as_deref().unwrap()
            )
        })
        .collect();
    expected_paths.sort();
    actual_paths.sort();
    assert_eq!(actual_paths.len(), FILES);
    assert_eq!(actual_paths, expected_paths);
    git_quiet(root, &["fsck", "--strict"]);

    // Allow blob loading, table preparation and result construction, but fewer
    // than even one allocation per cell of the 128 × 128 scoring matrix.
    assert!(
        allocations < FILES * FILES,
        "{allocations} allocations for {FILES} inexact renames; scoring must reuse tables"
    );

    let before = ALLOCATIONS.with(Cell::get);
    let copied = detect_copies_with_policy(
        dr.repo.odb(),
        None,
        added,
        50,
        true,
        &sources,
        &config,
        &policy,
    )
    .unwrap();
    let allocations = ALLOCATIONS.with(Cell::get) - before;
    eprintln!("{FILES} inexact copies: {allocations} allocations");
    let mut copied_paths: Vec<_> = copied
        .iter()
        .map(|entry| {
            assert_eq!(entry.status, DiffStatus::Copied);
            format!(
                "{}\t{}",
                entry.old_path.as_deref().unwrap(),
                entry.new_path.as_deref().unwrap()
            )
        })
        .collect();
    copied_paths.sort();
    assert_eq!(copied_paths, expected_paths);
    assert!(
        allocations < FILES * FILES,
        "{allocations} allocations for {FILES} inexact copies; scoring must reuse tables"
    );
}

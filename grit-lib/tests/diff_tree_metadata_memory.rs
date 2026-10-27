//! Tree metadata retained by a public diff must stay bounded across sibling trees.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};

struct LiveAllocator;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn record(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let bytes = live.get() + delta;
        live.set(bytes);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(bytes)));
    });
}

// SAFETY: All operations delegate to System with the caller's pointer and layout.
unsafe impl GlobalAlloc for LiveAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies a valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies a valid allocation layout.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(-(layout.size() as isize));
        // SAFETY: The pointer and layout came from System.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: The caller supplies the original allocation and a valid new size.
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() {
            record(size as isize - layout.size() as isize);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: LiveAllocator = LiveAllocator;

fn write_tree(repo: &Repository, entries: &[TreeEntry]) -> ObjectId {
    repo.odb()
        .write(
            ObjectKind::Tree,
            &grit_lib::objects::serialize_tree(entries),
        )
        .expect("write tree")
}

fn peak_for_width(width: usize) -> usize {
    const DIRS: usize = 400;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let old_blob = repo.odb().write(ObjectKind::Blob, b"old\n").expect("blob");
    let new_blob = repo.odb().write(ObjectKind::Blob, b"new\n").expect("blob");
    let mut old_root = Vec::new();
    let mut new_root = Vec::new();
    for d in 0..DIRS {
        // Distinct OIDs prevent repeated-subtree caching from hiding retention.
        let mut entries: Vec<_> = (0..width)
            .map(|f| TreeEntry {
                mode: 0o100644,
                name: format!("f{d:03}-{f:05}.txt").into_bytes(),
                oid: old_blob,
            })
            .collect();
        let old = write_tree(&repo, &entries);
        entries[0].oid = new_blob;
        let new = write_tree(&repo, &entries);
        old_root.push(TreeEntry {
            mode: 0o040000,
            name: format!("d{d:03}").into_bytes(),
            oid: old,
        });
        new_root.push(TreeEntry {
            oid: new,
            ..old_root.last().expect("directory").clone()
        });
    }
    let old = write_tree(&repo, &old_root);
    let new = write_tree(&repo, &new_root);
    drop((old_root, new_root, repo));
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("reopen without warmed caches");

    // This API is serial; TLS excludes unrelated test-runner allocations. Count
    // peak live bytes, not total allocations or allocator-dependent process RSS.
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let entries = repo
        .diff()
        .tree_to_tree(Some(&old), Some(&new))
        .expect("diff");
    let peak = (PEAK.with(Cell::get) - start) as usize;
    assert_eq!(entries.len(), DIRS);
    for (d, entry) in entries.iter().enumerate() {
        assert_eq!(entry.status.letter(), 'M');
        assert_eq!(entry.path(), format!("d{d:03}/f{d:03}-00000.txt"));
    }
    let with_results = LIVE.with(Cell::get) - start;
    drop(entries);
    let caches = LIVE.with(Cell::get) - start;
    eprintln!(
        "width={width}: peak={peak}, retained caches={caches}, result bytes={}",
        with_results - caches
    );
    peak
}

#[test]
fn completed_sibling_trees_do_not_accumulate_in_public_tree_diff() {
    let small = peak_for_width(128);
    let large = peak_for_width(512);
    eprintln!("tree metadata peak live bytes: 51,200 files={small}; 204,800 files={large}");
    // Changed paths stay at 400. Allow growth in the bounded loose-object cache
    // (larger individual trees), active frames, and allocator capacities, but not
    // retention of all 409,600 parsed entries across the old and new trees.
    assert!(
        large < small + 8 * 1024 * 1024,
        "completed sibling metadata accumulated: small={small}, large={large}"
    );
}

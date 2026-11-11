//! Tree metadata retained by a public diff must stay bounded across sibling trees.

use grit_lib::objects::FileMode;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{DiffEntry, RenameDetection, TreeDiffOptions};
use grit_lib::objects::{ObjectId, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
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

const DIRS: usize = 400;

fn write_tree(repo: &Repository, entries: &[TreeEntry]) -> ObjectId {
    repo.odb()
        .write(
            ObjectKind::Tree,
            &grit_lib::objects::serialize_tree(entries),
        )
        .expect("write tree")
}

/// `DIRS` directories of `width` files each; the first `changed` files of every directory
/// get new content. Returns the repository reopened without warmed caches.
fn metadata_fixture(
    width: usize,
    changed: usize,
) -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let old_blob = repo.odb().write(ObjectKind::Blob, b"old\n").expect("blob");
    let new_blob = repo.odb().write(ObjectKind::Blob, b"new\n").expect("blob");
    let mut old_root = Vec::new();
    let mut new_root = Vec::new();
    for d in 0..DIRS {
        // Distinct OIDs prevent repeated-subtree caching from hiding retention.
        let mut entries: Vec<_> = (0..width)
            .map(|f| {
                TreeEntry::new(
                    format!("f{d:03}-{f:05}.txt").into_bytes(),
                    FileMode::REGULAR,
                    old_blob,
                )
            })
            .collect();
        let old = write_tree(&repo, &entries);
        for entry in &mut entries[..changed] {
            entry.oid = new_blob;
        }
        let new = write_tree(&repo, &entries);
        old_root.push(TreeEntry::new(
            format!("d{d:03}").into_bytes(),
            FileMode::TREE,
            old,
        ));
        let mut changed_dir = old_root.last().expect("directory").clone();
        changed_dir.oid = new;
        new_root.push(changed_dir);
    }
    let old = write_tree(&repo, &old_root);
    let new = write_tree(&repo, &new_root);
    drop((old_root, new_root, repo));
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("reopen without warmed caches");
    (dir, repo, old, new)
}

/// Peak live bytes allocated on this thread while `f` runs, plus its result.
fn peak_during<T>(f: impl FnOnce() -> T) -> (usize, T) {
    // These APIs run serially; TLS excludes unrelated test-runner allocations. Count
    // peak live bytes, not total allocations or allocator-dependent process RSS.
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let value = f();
    ((PEAK.with(Cell::get) - start) as usize, value)
}

fn peak_for_width(width: usize) -> usize {
    let (_dir, repo, old, new) = metadata_fixture(width, 1);
    let start = LIVE.with(Cell::get);
    let (peak, entries) = peak_during(|| {
        repo.diff()
            .tree_to_tree(Some(&old), Some(&new), &TreeDiffOptions::default())
            .map(|r| r.entries)
            .expect("diff")
    });
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

#[test]
fn rename_pass_without_pairs_adds_no_copy_of_the_metadata_list() {
    // The `grit diff --name-status A B` path: rename detection is on, but a diff of
    // modifications has nothing to pair and must hand back the list it was given.
    let (_dir, repo, old, new) = metadata_fixture(64, 25);
    let run = |renames| {
        let options = TreeDiffOptions::default()
            .with_renames(renames)
            .with_threads(Some(ThreadPolicy::default()));
        peak_during(|| {
            repo.diff()
                .tree_to_tree(Some(&old), Some(&new), &options)
                .expect("diff")
                .entries
        })
    };
    // Warm the ODB caches and the config snapshot so both runs see the same reads.
    drop(run(RenameDetection::Off));
    drop(run(RenameDetection::Similarity(50)));
    let (plain_peak, plain) = run(RenameDetection::Off);
    let (renames_peak, renamed) = run(RenameDetection::Similarity(50));
    assert_eq!(renamed, plain);
    assert_eq!(renamed.len(), DIRS * 25);
    let list_bytes = plain.capacity() * std::mem::size_of::<DiffEntry>();
    eprintln!(
        "metadata list={list_bytes}, no renames peak={plain_peak}, renames peak={renames_peak}"
    );
    assert!(
        renames_peak < plain_peak + list_bytes / 8,
        "rename pass copied the metadata list: plain={plain_peak}, renames={renames_peak}, list={list_bytes}"
    );
}

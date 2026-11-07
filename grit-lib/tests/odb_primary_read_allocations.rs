//! Reads through a repository's files primary must not allocate for store dispatch.
//!
//! Each case counts heap allocations over repeated warm reads of the same objects and holds
//! them to the per-read count measured when the primary became an enum, so a change that
//! boxes the files store or adds an allocation on the primary read path fails here. Lower a
//! budget when a change removes allocations; never raise one to get a change through.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    // Count only this thread, and tolerate allocations during TLS teardown.
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

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

const BLOBS: usize = 64;
const ROUNDS: usize = 8;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).expect("utf8")
}

/// A repository with `BLOBS` committed files, optionally repacked so every object is packed.
fn fixture(packed: bool) -> (tempfile::TempDir, Repository, Vec<ObjectId>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    for i in 0..BLOBS {
        std::fs::write(
            root.join(format!("f{i:03}.txt")),
            format!("payload {i}\n").repeat(32),
        )
        .unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    if packed {
        git(root, &["repack", "-adq"]);
    }
    let oids = git(root, &["ls-files", "-s"])
        .lines()
        .map(|line| {
            let hex = line.split_whitespace().nth(1).expect("oid column");
            ObjectId::from_hex(hex).expect("oid")
        })
        .collect::<Vec<_>>();
    assert_eq!(oids.len(), BLOBS);
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (dir, repo, oids)
}

/// Allocations per read over `ROUNDS` warm passes of `read` across `oids`.
fn allocations_per_read(odb: &Odb, oids: &[ObjectId], read: impl Fn(&Odb, &ObjectId)) -> f64 {
    for oid in oids {
        read(odb, oid);
    }
    let before = allocations();
    for _ in 0..ROUNDS {
        for oid in oids {
            read(odb, oid);
        }
    }
    (allocations() - before) as f64 / (ROUNDS * oids.len()) as f64
}

fn check(label: &str, per_read: f64, budget: f64) {
    eprintln!("{label}: {per_read:.2} allocations per read (budget {budget})");
    assert!(
        per_read <= budget,
        "{label}: {per_read:.2} allocations per read exceeds the budget of {budget}"
    );
}

#[test]
fn packed_reads_through_the_files_primary_stay_within_budget() {
    let (_dir, repo, oids) = fixture(true);
    let odb = repo.odb();
    let mut buf = Vec::new();
    let buf = std::cell::RefCell::new(&mut buf);
    let into = allocations_per_read(odb, &oids, |odb, oid| {
        odb.read_into(oid, &mut buf.borrow_mut())
            .expect("read_into");
    });
    check("packed read_into", into, 1.0);
    let info = allocations_per_read(odb, &oids, |odb, oid| {
        odb.read_info(oid).expect("read_info");
    });
    check("packed read_info", info, 2.0);
    let contains = allocations_per_read(odb, &oids, |odb, oid| {
        assert!(odb.contains(oid).expect("contains"));
    });
    check("packed contains", contains, 10.0);
    let batch = odb.with_pack_read_context(|| {
        allocations_per_read(odb, &oids, |odb, oid| {
            odb.read_into(oid, &mut buf.borrow_mut())
                .expect("read_into");
        })
    });
    check("packed read_into in a pack read context", batch, 0.0);
}

#[test]
fn loose_reads_through_the_files_primary_stay_within_budget() {
    let (_dir, repo, oids) = fixture(false);
    let odb = repo.odb();
    let mut buf = Vec::new();
    let buf = std::cell::RefCell::new(&mut buf);
    let into = allocations_per_read(odb, &oids, |odb, oid| {
        odb.read_into(oid, &mut buf.borrow_mut())
            .expect("read_into");
    });
    check("loose read_into", into, 15.0);
    let info = allocations_per_read(odb, &oids, |odb, oid| {
        odb.read_info(oid).expect("read_info");
    });
    check("loose read_info", info, 14.0);
    let contains = allocations_per_read(odb, &oids, |odb, oid| {
        assert!(odb.contains(oid).expect("contains"));
    });
    check("loose contains", contains, 3.0);
}

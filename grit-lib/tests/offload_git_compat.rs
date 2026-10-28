//! What Git's own maintenance makes of a repository after [`offload_blobs`]: why
//! offload turns `repack.writeBitmaps` off.
//!
//! A reachability bitmap needs every object reachable from its commits in the packs
//! it covers. The offloaded blobs are in no pack of the main repository, and Git's
//! repack keeps promisor and other objects in separate packs, so once a push adds a
//! commit outside the promisor pack, Git can write neither a pack bitmap nor a
//! multi-pack one, and `git gc` on a bare repository (bitmaps on by default) fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_lib::error::Error;
use grit_lib::maintenance::{offload_blobs, OffloadError, OffloadOptions};
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::repo::Repository;
use grit_test_support::{git_cmd, Output};

const NOW: i64 = 4_000_000_000;

fn git_out(dir: &Path, args: &[&str]) -> Output {
    git_cmd(args)
        .in_dir(dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .exec()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(out.ok(), "git {args:?}: {}", out.stderr);
    out.stdout
}

/// Assert, without lazy fetches, that `repo` stores each of `revs` (`<rev>:<path>`
/// in `work`) and that `lop` stores each of `offloaded`. `git fsck` alone would not
/// do: a blob a promisor-pack tree names counts as promised, so fsck is clean
/// while it is gone.
fn assert_stored(work: &Path, repo: &Path, revs: &[&str], lop: &Path, offloaded: &[&str]) {
    for (dir, list) in [(repo, revs), (lop, offloaded)] {
        for rev in list {
            let oid = git(work, &["rev-parse", rev]);
            let present = git_out(dir, &["cat-file", "-e", oid.trim()]);
            assert!(
                present.ok(),
                "{rev} ({}) missing from {}",
                oid.trim(),
                dir.display()
            );
        }
    }
}

/// `len` bytes that do not compress.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// A bare `main.git` made by Git with a small file and a 200 KiB one, its large
/// blob offloaded to `lop.git`, and a push of one more commit into it.
fn offloaded_and_pushed(root: &Path) -> std::path::PathBuf {
    git(root, &["init", "-q", "-b", "main", "work"]);
    let work = root.join("work");
    std::fs::write(work.join("small.txt"), b"small\n").unwrap();
    std::fs::write(work.join("big.bin"), noise(1, 200 * 1024)).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(
        root,
        &["clone", "-q", "--bare", "--no-local", "work", "main.git"],
    );
    git(root, &["init", "-q", "--bare", "lop.git"]);
    let main_dir = root.join("main.git");
    git(&main_dir, &["remote", "add", "lop", "../lop.git"]);

    let main = Repository::open(&main_dir, &Default::default()).unwrap();
    let mut opts = OffloadOptions::new("lop", NOW);
    opts.filter = Some(ObjectFilterSpec::BlobLimit(100 * 1024));
    let report = offload_blobs(&main, &opts).unwrap();
    assert_eq!(report.dropped_from_main, 1);
    drop(main);
    clear_pack_cache();
    assert_eq!(
        git(&main_dir, &["config", "repack.writeBitmaps"]).trim(),
        "false"
    );

    std::fs::write(work.join("small.txt"), b"small, again\n").unwrap();
    git(&work, &["commit", "-q", "-am", "two"]);
    git(&work, &["push", "-q", "../main.git", "main"]);
    main_dir
}

/// Git keeps a blob a promisor-pack tree names only if it is in a promisor pack
/// itself: `repack -a -d` repacks the objects of promisor packs and leaves out
/// every other promised object, and `gc` does the same (moving them to a cruft
/// pack first, pruned later). Offload leaves the small blobs in its promisor pack
/// for that reason; this runs each of those through Git, after a push and again
/// after offload folds the push in.
#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn git_maintenance_keeps_every_blob_offload_leaves_in_main() {
    for maintenance in [&["repack", "-a", "-d"][..], &["gc", "--prune=now"], &["gc"]] {
        let tmp = tempfile::tempdir().unwrap();
        let main_dir = offloaded_and_pushed(tmp.path());
        let (work, lop) = (tmp.path().join("work"), tmp.path().join("lop.git"));
        let kept = ["HEAD:small.txt", "HEAD~1:small.txt"];
        git(&main_dir, maintenance);
        assert_stored(&work, &main_dir, &kept, &lop, &["HEAD:big.bin"]);

        let main = Repository::open(&main_dir, &Default::default()).unwrap();
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(100 * 1024));
        offload_blobs(&main, &opts).unwrap();
        drop(main);
        clear_pack_cache();
        git(&main_dir, maintenance);
        assert_stored(&work, &main_dir, &kept, &lop, &["HEAD:big.bin"]);
        git(&main_dir, &["fsck", "--no-dangling"]);
    }
}

#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn git_cannot_write_a_bitmap_once_an_offloaded_repository_takes_a_push() {
    let tmp = tempfile::tempdir().unwrap();
    let main_dir = offloaded_and_pushed(tmp.path());

    // With bitmaps on, as a bare repository has them by default, `git gc` fails.
    let gc = git_out(&main_dir, &["-c", "repack.writeBitmaps=true", "gc"]);
    assert!(!gc.ok(), "git gc with bitmaps succeeded:\n{}", gc.stderr);
    assert!(gc.stderr.contains("full closure"), "{}", gc.stderr);

    // A multi-pack bitmap over the promisor pack and the pushed one is no better: the
    // offloaded blob is in neither.
    let midx = git_out(&main_dir, &["repack", "-a", "-d", "-b", "--write-midx"]);
    assert!(!midx.ok(), "multi-pack bitmap written:\n{}", midx.stderr);
    assert!(midx.stderr.contains("full closure"), "{}", midx.stderr);

    // With the setting offload leaves, Git's maintenance runs, and its result checks.
    git(&main_dir, &["gc"]);
    git(&main_dir, &["fsck", "--no-dangling"]);
    let packs = std::fs::read_dir(main_dir.join("objects/pack")).unwrap();
    assert!(
        !packs
            .flatten()
            .any(|e| e.path().extension().is_some_and(|x| x == "bitmap")),
        "a bitmap was written"
    );
    // And it lost nothing: both versions of the small file are still in main, the
    // large one in the LOP.
    assert_stored(
        &tmp.path().join("work"),
        &main_dir,
        &["HEAD:small.txt", "HEAD~1:small.txt"],
        &tmp.path().join("lop.git"),
        &["HEAD:big.bin"],
    );
}

/// An ordinary pack with a `.keep` marker is left alone by offload, but a tree or
/// commit the promisor pack gains can name objects only that pack holds, and Git
/// then counts them as promised. Git's `repack -a -d` keeps promised objects only
/// in promisor packs, and leaves out of them whatever a kept pack holds, so once
/// the marker goes it deletes them, with nothing to fetch them back from. Offload
/// therefore refuses until the marker is gone, and then folds the pack in.
#[cfg_attr(
    not(feature = "full-tests"),
    ignore = "starts system git or is heavy; run with --features full-tests"
)]
#[test]
fn git_maintenance_keeps_objects_of_a_kept_pack_the_promisor_pack_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "work"]);
    let work = root.join("work");
    std::fs::write(work.join("small.txt"), b"small\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(
        root,
        &["clone", "-q", "--bare", "--no-local", "work", "main.git"],
    );
    let main_dir = root.join("main.git");
    let kept_pack = std::fs::read_dir(main_dir.join("objects/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let keep = kept_pack.with_extension("keep");
    std::fs::write(&keep, b"held by an operator\n").unwrap();

    std::fs::write(work.join("big.bin"), noise(2, 200 * 1024)).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "two"]);
    git(&work, &["push", "-q", "../main.git", "main"]);
    git(root, &["init", "-q", "--bare", "lop.git"]);
    git(&main_dir, &["remote", "add", "lop", "../lop.git"]);
    let mut opts = OffloadOptions::new("lop", NOW);
    opts.filter = Some(ObjectFilterSpec::BlobLimit(100 * 1024));
    let offload = |opts: &OffloadOptions| {
        let main = Repository::open(&main_dir, &Default::default()).unwrap();
        let result = offload_blobs(&main, opts);
        drop(main);
        clear_pack_cache();
        result
    };

    // The pushed commit and tree name the kept commit and blob.
    match offload(&opts) {
        Err(Error::Offload(OffloadError::KeptPackObjectPromised { pack, .. })) => {
            assert_eq!(pack.file_name(), kept_pack.file_name());
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        keep.is_file() && kept_pack.is_file(),
        "the kept pack was touched"
    );
    let lop = root.join("lop.git");
    assert_stored(&work, &main_dir, &["HEAD:big.bin"], &lop, &[]);

    std::fs::remove_file(&keep).unwrap();
    assert_eq!(offload(&opts).unwrap().dropped_from_main, 1);
    let kept = ["HEAD:small.txt", "HEAD~1", "HEAD~1^{tree}"];
    for _ in 0..2 {
        git(&main_dir, &["repack", "-a", "-d"]);
        assert_stored(&work, &main_dir, &kept, &lop, &["HEAD:big.bin"]);
    }
    git(&main_dir, &["fsck", "--no-dangling"]);
}

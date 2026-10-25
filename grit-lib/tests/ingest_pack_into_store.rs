//! Applying received packs to custom object stores (issue #1892).
//!
//! Packs are produced by the system `git` (full, ref-delta, ofs-delta and thin) and fed
//! to [`ingest_pack_into`] or to the repository-level ingest entry points on a
//! repository whose primary is a [`MemoryStore`]. Every object must come back readable
//! and hash to its id, and a pack built from the custom store must pass
//! `git index-pack --strict`.

use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::index_pack::{
    ingest_received_pack, ingest_received_pack_path, IngestPackOptions,
};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::pack_objects::{build_pack, PackBuildOptions};
use grit_lib::objects::unpack_objects::{
    ingest_pack_into, ingest_pack_into_with_progress, pack_is_thin, UnpackOptions,
};
use grit_lib::objects::{hash::hash_object, HashAlgo, ObjectId};
use grit_lib::progress::{FlagCancel, ProgressPhase, ProgressSink};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{Repository, RepositoryBuilder};

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn git_with_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = git_cmd(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim()).expect("valid oid")
}

/// A file large enough for `git pack-objects` to store later versions as deltas.
fn big_text(version: usize) -> String {
    (0..400)
        .map(|i| {
            if i == 200 {
                format!("line {i} changed in version {version}\n")
            } else {
                format!("line {i} with some stable text so deltas pay off\n")
            }
        })
        .collect()
}

/// Upstream with `commits` commits, each rewriting one line of `big.txt`.
fn git_history(commits: usize, object_format: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(
        dir.path(),
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={object_format}"),
            ".",
        ],
    );
    for version in 0..commits {
        fs::write(dir.path().join("big.txt"), big_text(version)).expect("write");
        fs::write(dir.path().join("small.txt"), format!("v{version}\n")).expect("write");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", &format!("c{version}")]);
    }
    dir
}

fn pack_objects(dir: &Path, revs: &str, extra: &[&str]) -> Vec<u8> {
    let mut args = vec!["pack-objects", "--revs", "--stdout", "-q"];
    args.extend_from_slice(extra);
    git_with_stdin(dir, &args, revs.as_bytes())
}

fn rev_list_objects(dir: &Path, range: &[&str]) -> HashSet<ObjectId> {
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(range);
    git(dir, &args)
        .lines()
        .map(|line| oid(line.split(' ').next().unwrap_or(line)))
        .collect()
}

/// Every object in `expected` is in `store` and hashes back to its id.
fn assert_store_holds(store: &dyn ObjectStore, expected: &HashSet<ObjectId>) {
    for id in expected {
        let obj = store
            .read(id)
            .expect("read")
            .unwrap_or_else(|| panic!("{id} missing from store"));
        let rehashed = hash_object(store.hash_algo(), obj.kind, &obj.data).expect("hash");
        assert_eq!(&rehashed, id, "{id} stored with the wrong content");
    }
}

/// Count of pack entries by raw type code (6 = ofs-delta, 7 = ref-delta) via `git verify-pack`.
fn delta_entry_count(pack: &[u8]) -> usize {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("probe.pack");
    fs::write(&path, pack).expect("write pack");
    git(
        dir.path(),
        &["index-pack", path.to_str().expect("utf8 path")],
    );
    git(
        dir.path(),
        &["verify-pack", "-v", path.to_str().expect("utf8 path")],
    )
    .lines()
    .filter(|line| line.split_whitespace().count() >= 7 && !line.contains("chain length"))
    .count()
}

fn custom_primary_repo(git_dir: &Path, store: Arc<MemoryStore>) -> Repository {
    fs::create_dir_all(git_dir).expect("git dir");
    RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store)
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(git_dir, None)
        .expect("open custom repository")
}

#[test]
fn ingest_full_pack_with_ofs_and_ref_deltas() {
    let upstream = git_history(4, "sha1");
    let expected = rev_list_objects(upstream.path(), &["main"]);
    for extra in [&["--delta-base-offset"][..], &[][..]] {
        let pack = pack_objects(upstream.path(), "main\n", extra);
        assert!(
            delta_entry_count(&pack) > 0,
            "fixture must contain deltas ({extra:?})"
        );
        let store = MemoryStore::new(HashAlgo::Sha1);
        let ingested = ingest_pack_into(
            &mut Cursor::new(&pack),
            &store,
            &store,
            &UnpackOptions::default(),
        )
        .expect("ingest");
        assert_eq!(ingested.object_ids, expected);
        assert_eq!(ingested.pack_bytes, pack.len() as u64);
        assert_eq!(ingested.object_counts.commits, 4);
        assert_eq!(ingested.object_counts.trees, 4);
        assert_eq!(ingested.object_counts.blobs, 8);
        assert_eq!(ingested.object_counts.tags, 0);
        assert_store_holds(&store, &expected);
    }
}

#[test]
fn ingest_thin_pack_resolves_bases_already_in_the_store() {
    let upstream = git_history(3, "sha1");
    let store = MemoryStore::new(HashAlgo::Sha1);
    let base = pack_objects(upstream.path(), "main~1\n", &["--delta-base-offset"]);
    ingest_pack_into(
        &mut Cursor::new(&base),
        &store,
        &store,
        &UnpackOptions::default(),
    )
    .expect("ingest base");

    let thin = pack_objects(upstream.path(), "main\n^main~1\n", &["--thin"]);
    assert!(
        pack_is_thin(&thin, HashAlgo::Sha1),
        "fixture must be a thin pack"
    );
    let new_objects = rev_list_objects(upstream.path(), &["main", "^main~1"]);
    let ingested = ingest_pack_into(
        &mut Cursor::new(&thin),
        &store,
        &store,
        &UnpackOptions {
            strict: true,
            ..UnpackOptions::default()
        },
    )
    .expect("ingest thin pack");
    assert_eq!(ingested.object_ids, new_objects);
    assert_store_holds(&store, &rev_list_objects(upstream.path(), &["main"]));
}

#[test]
fn thin_pack_bases_come_from_the_bases_view() {
    let upstream = git_history(2, "sha1");
    let bases = MemoryStore::new(HashAlgo::Sha1);
    let base = pack_objects(upstream.path(), "main~1\n", &[]);
    ingest_pack_into(
        &mut Cursor::new(&base),
        &bases,
        &bases,
        &UnpackOptions::default(),
    )
    .expect("ingest base");
    let thin = pack_objects(upstream.path(), "main\n^main~1\n", &["--thin"]);
    assert!(pack_is_thin(&thin, HashAlgo::Sha1));

    let target = MemoryStore::new(HashAlgo::Sha1);
    let err = ingest_pack_into(
        &mut Cursor::new(&thin),
        &target,
        &target,
        &UnpackOptions::default(),
    )
    .expect_err("thin pack without its bases");
    assert!(
        matches!(&err, Error::UnresolvedDeltas { count } if *count > 0),
        "unexpected error: {err:?}"
    );

    let target = MemoryStore::new(HashAlgo::Sha1);
    let ingested = ingest_pack_into(
        &mut Cursor::new(&thin),
        &target,
        &bases,
        &UnpackOptions::default(),
    )
    .expect("thin pack with bases view");
    assert_store_holds(&target, &ingested.object_ids);
    let mut stored = 0usize;
    target
        .for_each_object(&mut |_| {
            stored += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("enumerate");
    assert_eq!(
        stored,
        ingested.object_ids.len(),
        "bases are read, never copied into the target"
    );
}

#[test]
fn strict_ingest_rejects_a_pack_with_dangling_references() {
    let upstream = git_history(2, "sha1");
    // Commits and trees only: the blobs the trees name are not in the pack or the store.
    let ids: String = git(upstream.path(), &["rev-list", "--objects", "main"])
        .lines()
        .filter_map(|line| {
            let id = line.split(' ').next()?;
            let kind = git(upstream.path(), &["cat-file", "-t", id]);
            (kind.trim() != "blob").then(|| format!("{id}\n"))
        })
        .collect();
    let pack = git_with_stdin(
        upstream.path(),
        &["pack-objects", "--stdout", "-q"],
        ids.as_bytes(),
    );
    let store = MemoryStore::new(HashAlgo::Sha1);
    let err = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions {
            strict: true,
            ..UnpackOptions::default()
        },
    )
    .expect_err("strict ingest must reject missing blobs");
    assert!(
        matches!(&err, Error::CorruptObject(msg) if msg.starts_with("strict:")),
        "unexpected error: {err:?}"
    );

    let lenient = MemoryStore::new(HashAlgo::Sha1);
    ingest_pack_into(
        &mut Cursor::new(&pack),
        &lenient,
        &lenient,
        &UnpackOptions::default(),
    )
    .expect("non-strict ingest accepts the same pack");
}

#[test]
fn corrupt_and_truncated_packs_are_rejected() {
    let upstream = git_history(2, "sha1");
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let store = MemoryStore::new(HashAlgo::Sha1);

    let mut bad_trailer = pack.clone();
    let last = bad_trailer.len() - 1;
    bad_trailer[last] ^= 0x01;
    let err = ingest_pack_into(
        &mut Cursor::new(&bad_trailer),
        &store,
        &store,
        &UnpackOptions::default(),
    )
    .expect_err("checksum mismatch");
    assert!(matches!(err, Error::CorruptObject(_)), "{err:?}");

    let truncated = &pack[..pack.len() / 2];
    let err = ingest_pack_into(
        &mut Cursor::new(truncated),
        &store,
        &store,
        &UnpackOptions::default(),
    )
    .expect_err("truncated pack");
    assert!(
        matches!(err, Error::CorruptObject(_) | Error::Zlib(_)),
        "{err:?}"
    );

    let err = ingest_pack_into(
        &mut Cursor::new(b"NOPE\0\0\0\x02\0\0\0\0".to_vec()),
        &store,
        &store,
        &UnpackOptions::default(),
    )
    .expect_err("not a pack");
    assert!(matches!(err, Error::CorruptObject(_)), "{err:?}");

    let err = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions {
            max_input_bytes: Some(32),
            ..UnpackOptions::default()
        },
    )
    .expect_err("max input size");
    assert!(matches!(err, Error::CorruptObject(_)), "{err:?}");
}

#[test]
fn dry_run_hashes_without_writing() {
    let upstream = git_history(2, "sha1");
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let store = MemoryStore::new(HashAlgo::Sha1);
    let ingested = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions {
            dry_run: true,
            ..UnpackOptions::default()
        },
    )
    .expect("dry run");
    assert_eq!(
        ingested.object_ids,
        rev_list_objects(upstream.path(), &["main"])
    );
    let mut stored = 0usize;
    store
        .for_each_object(&mut |_| {
            stored += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("enumerate");
    assert_eq!(stored, 0);
}

#[derive(Default)]
struct PhaseRecorder {
    started: Vec<(ProgressPhase, Option<u64>)>,
    increments: u64,
    finished: usize,
}

impl ProgressSink for PhaseRecorder {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.started.push((phase, total));
    }
    fn inc(&mut self, units: u64) {
        self.increments += units;
    }
    fn finish(&mut self) {
        self.finished += 1;
    }
}

#[test]
fn progress_reports_indexing_and_resolving_and_cancel_stops() {
    let upstream = git_history(3, "sha1");
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let total = rev_list_objects(upstream.path(), &["main"]).len() as u64;
    let store = MemoryStore::new(HashAlgo::Sha1);
    let mut progress = PhaseRecorder::default();
    ingest_pack_into_with_progress(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions::default(),
        Some(&mut progress),
        None,
    )
    .expect("ingest");
    assert_eq!(
        progress.started[0],
        (ProgressPhase::IndexingObjects, Some(total))
    );
    assert_eq!(progress.started[1].0, ProgressPhase::ResolvingDeltas);
    let deltas = progress.started[1].1.expect("delta total");
    assert!(deltas > 0);
    assert_eq!(progress.increments, total + deltas);
    assert_eq!(progress.finished, 2);

    let cancel = FlagCancel::new();
    cancel.cancel();
    let fresh = MemoryStore::new(HashAlgo::Sha1);
    let err = ingest_pack_into_with_progress(
        &mut Cursor::new(&pack),
        &fresh,
        &fresh,
        &UnpackOptions::default(),
        None,
        Some(&cancel),
    )
    .expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
}

#[test]
fn sha256_pack_ingests_into_a_sha256_store() {
    let upstream = git_history(2, "sha256");
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let store = MemoryStore::new(HashAlgo::Sha256);
    let ingested = ingest_pack_into(
        &mut Cursor::new(&pack),
        &store,
        &store,
        &UnpackOptions::default(),
    )
    .expect("ingest sha256");
    let expected = rev_list_objects(upstream.path(), &["main"]);
    assert_eq!(ingested.object_ids, expected);
    assert_store_holds(&store, &expected);
}

/// The reproduction from the issue: `ingest_received_pack` on a repository whose primary
/// is a [`MemoryStore`] used to fail with `UnsupportedObjectStore`.
#[test]
fn received_pack_lands_in_custom_primary_and_round_trips_through_git() {
    let upstream = git_history(3, "sha1");
    let tip = oid(&git(upstream.path(), &["rev-parse", "main"]));
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let scratch = tempfile::tempdir().expect("tempdir");
    let git_dir = scratch.path().join("custom.git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = custom_primary_repo(&git_dir, Arc::clone(&store));

    let ingested =
        ingest_received_pack(pack, repo.odb(), &IngestPackOptions::default()).expect("ingest");
    let expected = rev_list_objects(upstream.path(), &["main"]);
    assert_eq!(ingested.object_ids, expected);
    assert!(ingested.pack_path.is_none());
    assert_eq!(ingested.object_counts.commits, 3);
    assert_store_holds(store.as_ref(), &expected);
    assert!(repo.odb().read(&tip).is_ok());
    assert!(
        !git_dir.join("objects").exists(),
        "nothing may be written under objects/ for a custom primary"
    );

    // Outbound half: a pack built from the custom store is accepted by git.
    let rebuilt = build_pack(repo.odb(), &[tip], &[], &PackBuildOptions::default()).expect("pack");
    let check = tempfile::tempdir().expect("tempdir");
    git(check.path(), &["init", "-q", "--bare", "."]);
    git_with_stdin(
        check.path(),
        &["index-pack", "--stdin", "--strict"],
        &rebuilt,
    );
    git(
        check.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );
    git(check.path(), &["fsck", "--full", "--strict"]);
    assert_eq!(rev_list_objects(check.path(), &["main"]), expected);
}

#[test]
fn received_pack_path_is_consumed_for_custom_primary() {
    let upstream = git_history(2, "sha1");
    let pack = pack_objects(upstream.path(), "main\n", &[]);
    let scratch = tempfile::tempdir().expect("tempdir");
    let spool = scratch.path().join("received.pack");
    fs::write(&spool, &pack).expect("write spool");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = custom_primary_repo(&scratch.path().join("custom.git"), Arc::clone(&store));

    let ingested =
        ingest_received_pack_path(spool.clone(), repo.odb(), &IngestPackOptions::default())
            .expect("ingest path");
    assert!(!spool.exists(), "the received pack file is consumed");
    assert_eq!(ingested.pack_bytes, pack.len() as u64);
    assert_store_holds(
        store.as_ref(),
        &rev_list_objects(upstream.path(), &["main"]),
    );

    let empty = ingest_received_pack(Vec::new(), repo.odb(), &IngestPackOptions::default())
        .expect("empty pack");
    assert!(empty.object_ids.is_empty());
    assert!(empty.pack_path.is_none());
}

#[test]
fn files_primary_ingest_still_installs_a_pack() {
    let upstream = git_history(2, "sha1");
    let pack = pack_objects(upstream.path(), "main\n", &["--delta-base-offset"]);
    let pack_len = pack.len() as u64;
    let scratch = tempfile::tempdir().expect("tempdir");
    git(scratch.path(), &["init", "-q", "--bare", "."]);
    let repo = Repository::open(scratch.path(), &RepositoryOptions::empty()).expect("open");
    let ingested =
        ingest_received_pack(pack, repo.odb(), &IngestPackOptions::default()).expect("ingest");
    let pack_path = ingested.pack_path.expect("installed pack");
    assert!(pack_path.starts_with(scratch.path().join("objects").join("pack")));
    assert_eq!(ingested.pack_bytes, pack_len);
    assert_eq!(
        fs::metadata(&pack_path).expect("meta").len(),
        ingested.pack_bytes
    );
    assert!(pack_path.with_extension("idx").is_file());
    let tip = git(upstream.path(), &["rev-parse", "main"]);
    git(
        scratch.path(),
        &["update-ref", "refs/heads/main", tip.trim()],
    );
    git(scratch.path(), &["fsck", "--full", "--strict"]);
}

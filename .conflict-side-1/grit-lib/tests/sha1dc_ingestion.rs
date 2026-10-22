//! SHA-1 collision detection on object ingestion paths (sha1dc).

use std::io::Write;
use std::num::NonZeroUsize;
use std::process::{Command, Stdio};

use grit_lib::error::Error;
use grit_lib::maintenance::repository_fsck::{
    fsck, issue_msg_id, FsckIssue, FsckOptions, FsckReport,
};
use grit_lib::objects::hash::{
    hash_object, hash_objects_parallel, try_par_hash_with_force, ParallelHashError,
};
use grit_lib::objects::index_pack::{install_pack_bytes, IngestPackOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::verify_pack_and_collect;
use grit_lib::objects::unpack_objects::{unpack_objects, UnpackOptions};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{
    git_fsck, HashAlgo as FixtureAlgo, ObjectKind as PackKind, PackBuilder, RepoFixture,
};

fn shattered_prefix(which: u8) -> Vec<u8> {
    let name = format!("shattered-{which}-prefix.bin");
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .expect("fixture")
}

fn assert_hash_collision<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::HashCollision { .. })));
}

fn shattered_blob_store_bytes(which: u8) -> Vec<u8> {
    let payload = shattered_prefix(which);
    let mut store = format!("blob {}\0", payload.len()).into_bytes();
    store.extend_from_slice(&payload);
    store
}

/// The blob id `git hash-object` assigns to `data` (without writing it).
fn git_blob_id(fixture: &RepoFixture, data: &[u8]) -> ObjectId {
    let mut child = Command::new("git")
        .current_dir(fixture.path())
        .args(["hash-object", "-t", "blob", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(data)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git hash-object must accept the blob");
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid")
}

fn assert_git_fsck_strict(fixture: &RepoFixture) {
    let fsck = git_fsck(fixture.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
}

// Git's own history stores the SHAttered PDFs as blobs (`t/t0013/shattered-*.pdf`).
// Only their raw SHA-1 collides; with the object header in front the colliding blocks
// are shifted, so Git accepts them and every grit ingestion path must too.

#[test]
fn raw_shattered_prefix_digest_is_detected() {
    for which in [1, 2] {
        let data = shattered_prefix(which);
        assert_hash_collision(HashAlgo::Sha1.digest(&data));
    }
}

#[test]
fn odb_write_raw_accepts_shattered_prefix_blob_like_git() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let git_oid = git_blob_id(&repo, &shattered_prefix(1));
    assert_eq!(
        odb.write_raw(&shattered_blob_store_bytes(1))
            .expect("write_raw"),
        git_oid
    );
    let git_oid = git_blob_id(&repo, &shattered_prefix(2));
    assert_eq!(
        odb.write_raw_local(&shattered_blob_store_bytes(2))
            .expect("write_raw_local"),
        git_oid
    );
    assert_git_fsck_strict(&repo);
}

#[test]
fn odb_write_accepts_shattered_prefix_blob_like_git() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let data = shattered_prefix(1);
    let git_oid = git_blob_id(&repo, &data);
    assert_hash_collision(HashAlgo::Sha1.digest(&data));
    assert_eq!(odb.hash(ObjectKind::Blob, &data).expect("hash"), git_oid);
    assert_eq!(odb.write(ObjectKind::Blob, &data).expect("write"), git_oid);
    assert_eq!(odb.read(&git_oid).expect("read back").data, data);
    assert_git_fsck_strict(&repo);
}

#[test]
fn index_pack_accepts_pack_with_shattered_prefix_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let data = shattered_prefix(1);
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &data);
    let pack = builder.build().bytes;
    let ingested =
        install_pack_bytes(pack, &odb, &IngestPackOptions::default()).expect("index-pack");
    assert!(ingested.object_ids.contains(&git_blob_id(&repo, &data)));
    assert_git_fsck_strict(&repo);
}

#[test]
fn unpack_objects_accepts_shattered_prefix_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let data = shattered_prefix(2);
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &data);
    let pack = builder.build().bytes;
    let unpacked =
        unpack_objects(&mut pack.as_slice(), &odb, &UnpackOptions::default()).expect("unpack");
    assert_eq!(unpacked, 1);
    assert_eq!(
        odb.read(&git_blob_id(&repo, &data)).expect("read").data,
        data
    );
    assert_git_fsck_strict(&repo);
}

#[test]
fn fsck_accepts_git_written_shattered_prefix_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let data = shattered_prefix(1);
    let hex = grit_test_support::write_loose_object(
        repo.odb().objects_dir(),
        FixtureAlgo::Sha1,
        "blob",
        &data,
    )
    .expect("write loose bytes at colliding oid path");
    let oid = grit_lib::objects::ObjectId::from_hex(&hex).expect("written oid hex");
    assert_eq!(
        hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect("blob id"),
        oid
    );

    let report = fsck(
        &repo,
        &FsckOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    assert_no_collision_finding(&report, &oid);
}

fn assert_no_collision_finding(report: &FsckReport, oid: &grit_lib::objects::ObjectId) {
    assert!(
        !report.issues.iter().any(|issue| {
            issue_msg_id(issue) == "sha1Collision"
                || matches!(issue, FsckIssue::BadObject { oid: found, .. } if found == oid)
        }),
        "unexpected findings for {oid}: {:?}",
        report.issues.iter().map(issue_msg_id).collect::<Vec<_>>()
    );
}

#[test]
fn parallel_hash_surfaces_collision() {
    let data = shattered_prefix(1);
    let items = [data.as_slice(), b"plain".as_slice()];
    let threads = NonZeroUsize::new(2).expect("threads");
    let err = try_par_hash_with_force(&items, threads, |bytes| HashAlgo::Sha1.digest(bytes))
        .expect_err("parallel");
    match err {
        ParallelHashError::Task(Error::HashCollision { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }

    let blobs = [(ObjectKind::Blob, data.as_slice())];
    let oids = hash_objects_parallel(HashAlgo::Sha1, &blobs, threads).expect("blob ids");
    assert_eq!(
        oids,
        vec![hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect("blob id")]
    );
}

#[test]
fn grit_written_loose_and_packs_pass_git_fsck_strict() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&fixture.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"safe payload").expect("write");
    assert!(!oid.to_hex().is_empty());

    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, b"pack-safe");
    let pack = builder.build().bytes;
    install_pack_bytes(pack, &odb, &IngestPackOptions::default()).expect("install pack");

    let fsck = git_fsck(fixture.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
}

#[test]
fn git_written_pack_indexes_and_fscks_clean_with_grit() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    std::fs::write(fixture.path().join("file.txt"), b"hello").expect("file");
    let add = fixture.git(&["add", "file.txt"]);
    assert!(add.ok, "git add: {}", add.stderr);
    let commit = fixture.git(&["commit", "-m", "seed"]);
    assert!(commit.ok, "git commit: {}", commit.stderr);
    let repack = fixture.git(&["repack", "-a", "-d"]);
    assert!(repack.ok, "git repack: {}", repack.stderr);
    let pack_dir = fixture.objects_dir().join("pack");
    let idx_path = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find_map(|e| {
            let p = e.path();
            p.extension().is_some_and(|x| x == "idx").then_some(p)
        })
        .expect("pack idx");
    verify_pack_and_collect(&idx_path).expect("grit verify pack");
}

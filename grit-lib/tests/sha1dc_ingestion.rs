//! SHA-1 collision detection on object ingestion paths (sha1dc).

use std::io::Write;
use std::num::NonZeroUsize;
use std::process::{Command, Stdio};

use grit_lib::error::Error;
use grit_lib::maintenance::repository_fsck::{
    fsck, issue_msg_id, FsckIssue, FsckOptions, FsckReport,
};
use grit_lib::objects::hash::{hash_object, try_hash_objects_parallel, ParallelHashError};
use grit_lib::objects::index_pack::{install_pack_bytes, IngestPackOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::verify_pack_and_collect;
use grit_lib::objects::unpack_objects::{unpack_objects, UnpackOptions};
use grit_lib::objects::{HashAlgo, ObjectKind};
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

#[test]
fn odb_write_raw_rejects_shattered_store_bytes() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let store = shattered_blob_store_bytes(1);
    let err = odb
        .write_raw(&store)
        .expect_err("write_raw must reject collision");
    assert!(matches!(err, Error::HashCollision { .. }));
}

#[test]
fn odb_write_raw_local_rejects_shattered_store_bytes() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let store = shattered_blob_store_bytes(2);
    let err = odb
        .write_raw_local(&store)
        .expect_err("write_raw_local must reject collision");
    assert!(matches!(err, Error::HashCollision { .. }));
}

#[test]
fn odb_write_rejects_shattered_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let data = shattered_prefix(1);
    assert_hash_collision(HashAlgo::Sha1.try_digest(&data));
    assert_hash_collision(odb.try_hash(ObjectKind::Blob, &data));
    let err = odb
        .write(ObjectKind::Blob, &data)
        .expect_err("write must reject when object id hashing detects collision");
    assert!(matches!(err, Error::HashCollision { .. }));
}

#[test]
fn index_pack_rejects_pack_with_shattered_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &shattered_prefix(1));
    let pack = builder.build().bytes;
    let err =
        install_pack_bytes(pack, &odb, &IngestPackOptions::default()).expect_err("index-pack");
    assert!(matches!(err, Error::HashCollision { .. }));
}

#[test]
fn unpack_objects_rejects_shattered_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &shattered_prefix(2));
    let pack = builder.build().bytes;
    let err =
        unpack_objects(&mut pack.as_slice(), &odb, &UnpackOptions::default()).expect_err("unpack");
    assert!(matches!(err, Error::HashCollision { .. }));
}

#[test]
fn fsck_reports_shattered_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let data = shattered_prefix(1);
    let oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
    grit_test_support::write_loose_object(
        repo.odb().objects_dir(),
        FixtureAlgo::Sha1,
        "blob",
        &data,
    )
    .expect("write loose bytes at colliding oid path");

    let report = fsck(
        &repo,
        &FsckOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    assert_shattered_fsck_finding(&report, &oid);
}

fn assert_shattered_fsck_finding(report: &FsckReport, oid: &grit_lib::objects::ObjectId) {
    assert!(
        report.issues.iter().any(|issue| {
            issue_msg_id(issue) == "sha1Collision"
                && matches!(issue, FsckIssue::BadObject { oid: found, .. } if found == oid)
        }),
        "expected sha1Collision for {oid}, got {:?}",
        report.issues.iter().map(issue_msg_id).collect::<Vec<_>>()
    );
}

#[test]
fn parallel_hash_surfaces_collision() {
    let data = shattered_prefix(1);
    let items = [(ObjectKind::Blob, data.as_slice())];
    let threads = NonZeroUsize::new(2).expect("threads");
    let err = try_hash_objects_parallel(HashAlgo::Sha1, &items, threads).expect_err("parallel");
    match err {
        ParallelHashError::Task(Error::HashCollision { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn grit_stricter_than_git_on_shattered_prefix_blob() {
    let data = shattered_prefix(1);
    assert_hash_collision(HashAlgo::Sha1.try_hash_object(ObjectKind::Blob, &data));
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["hash-object", "-w", "-t", "blob", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&data)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    if !out.status.success() {
        assert!(out.stdout.is_empty());
    }
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

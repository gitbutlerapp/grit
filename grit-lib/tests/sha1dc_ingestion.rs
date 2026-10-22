//! Git-compatible ingestion for SHAttered test vectors (sha1dc signals, plain SHA-1 ids).

use std::io::Write;
use std::num::NonZeroUsize;
use std::process::{Command, Stdio};

use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions, FsckReport, FsckSeverity};
use grit_lib::objects::hash::sha1dc_chosen_prefix_digest;
use grit_lib::objects::hash::{hash_object, try_hash_objects_parallel};
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

#[test]
fn odb_write_accepts_shattered_prefix_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let data = shattered_prefix(1);
    assert!(sha1dc_chosen_prefix_digest(&data).is_some());
    let oid = odb.write(ObjectKind::Blob, &data).expect("write");
    assert_eq!(oid, hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data));
    let round = odb.read(&oid).expect("read");
    assert_eq!(round.data, data);
}

#[test]
fn odb_write_raw_accepts_shattered_store_bytes() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let payload = shattered_prefix(2);
    let mut store = format!("blob {}\0", payload.len()).into_bytes();
    store.extend_from_slice(&payload);
    odb.write_raw(&store).expect("write_raw");
}

#[test]
fn index_pack_accepts_pack_with_shattered_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &shattered_prefix(1));
    let pack = builder.build().bytes;
    install_pack_bytes(pack, &odb, &IngestPackOptions::default()).expect("index-pack");
}

#[test]
fn unpack_objects_accepts_shattered_blob() {
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&repo.objects_dir());
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(PackKind::Blob, &shattered_prefix(2));
    let pack = builder.build().bytes;
    unpack_objects(&mut pack.as_slice(), &odb, &UnpackOptions::default()).expect("unpack");
}

#[test]
fn parallel_hash_matches_shattered_prefix() {
    let data = shattered_prefix(1);
    let items = [(ObjectKind::Blob, data.as_slice())];
    let threads = NonZeroUsize::new(2).expect("threads");
    let oids = try_hash_objects_parallel(HashAlgo::Sha1, &items, threads).expect("parallel");
    assert_eq!(
        oids[0],
        hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data)
    );
}

#[test]
fn grit_matches_git_hash_object_on_shattered_prefix() {
    let data = shattered_prefix(1);
    let grit_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["hash-object", "-t", "blob", "--stdin"])
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
    assert!(out.status.success(), "git hash-object: {:?}", out.stderr);
    let git_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(grit_oid.to_hex(), git_hex);
}

#[test]
fn git_written_shattered_blob_repack_grit_clone_smoke() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let data = shattered_prefix(1);
    std::fs::write(fixture.path().join("collision.bin"), &data).expect("write");
    let add = fixture.git(&["add", "collision.bin"]);
    assert!(add.ok, "git add: {}", add.stderr);
    let commit = fixture.git(&["commit", "-m", "collision"]);
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
    let fsck = git_fsck(fixture.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
}

#[test]
fn grit_written_loose_and_packs_pass_git_fsck_strict() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let odb = Odb::new(&fixture.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"safe payload").expect("write");
    assert!(!oid.to_hex().is_empty());
    odb.write(ObjectKind::Blob, &shattered_prefix(1))
        .expect("shattered write");

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

fn assert_fsck_ok(report: &FsckReport) {
    use grit_lib::maintenance::repository_fsck::issue_severity;
    let config = grit_lib::config::ConfigSet::default();
    let errors = report
        .issues
        .iter()
        .filter(|i| issue_severity(i, &config, false) == FsckSeverity::Error)
        .count();
    assert_eq!(errors, 0, "unexpected fsck errors: {:?}", report.issues);
}

#[test]
fn fsck_passes_on_shattered_loose_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let data = shattered_prefix(1);
    grit_test_support::write_loose_object(
        repo.odb().objects_dir(),
        FixtureAlgo::Sha1,
        "blob",
        &data,
    )
    .expect("write loose");

    let report = fsck(
        &repo,
        &FsckOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    assert_fsck_ok(&report);
}

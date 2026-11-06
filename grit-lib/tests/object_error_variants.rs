//! Typed object-store errors: damaged fixtures from system git, variant assertions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::error::Error;
use grit_lib::objects::error::ObjectError;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::read_pack_index;
use grit_lib::objects::ObjectId;
use grit_test_support::objects::{write_loose_object, HashAlgo as FixtureAlgo, RepoFixture};

fn init_repo() -> RepoFixture {
    RepoFixture::init(FixtureAlgo::Sha1).expect("git init")
}

#[test]
fn corrupt_loose_object_truncated_bytes() {
    let repo = init_repo();
    let hex = write_loose_object(
        &repo.path().join(".git/objects"),
        FixtureAlgo::Sha1,
        "blob",
        b"hello",
    )
    .expect("write loose");
    let oid = ObjectId::from_hex(&hex).expect("parse oid");
    let path = repo
        .path()
        .join(".git/objects")
        .join(format!("{}/{}", &hex[..2], &hex[2..]));
    let mut bytes = fs::read(&path).expect("read loose");
    bytes.truncate(bytes.len().saturating_sub(4));
    fs::write(&path, bytes).expect("truncate");

    let odb = Odb::new(&repo.path().join(".git/objects"));
    let err = odb.read(&oid).expect_err("truncated loose object");
    match err {
        Error::Object(ObjectError::CorruptLooseObject { oid: got, .. }) => {
            assert_eq!(got, oid);
        }
        Error::CorruptLooseObject { oid: got, .. } => {
            assert_eq!(got, oid);
        }
        other => panic!("expected corrupt loose object, got {other:?}"),
    }
}

#[test]
fn pack_index_checksum_mismatch_flipped_trailer() {
    let repo = init_repo();
    repo.git(&["commit", "--allow-empty", "-m", "seed"]);
    repo.git(&["gc", "--prune=now"]);
    let objects = repo.path().join(".git/objects/pack");
    let idx = fs::read_dir(&objects)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack idx");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&idx, fs::Permissions::from_mode(0o644)).expect("chmod idx");
    }
    let mut bytes = fs::read(&idx).expect("read idx");
    if let Some(last) = bytes.last_mut() {
        *last ^= 0xff;
    }
    fs::write(&idx, bytes).expect("flip checksum");

    let err = read_pack_index(&idx).expect_err("bad idx checksum");
    match err {
        Error::Object(ObjectError::PackIndexChecksumMismatch { .. }) => {}
        Error::PackIndexChecksumMismatch { .. } => {}
        other => panic!("expected pack index checksum error, got {other:?}"),
    }
}

#[test]
fn object_not_found_missing_oid() {
    let repo = init_repo();
    let odb = Odb::new(&repo.path().join(".git/objects"));
    let missing = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid");
    let err = odb.read(&missing).expect_err("missing object");
    match err {
        Error::Object(ObjectError::ObjectNotFound { oid, .. }) => {
            assert_eq!(oid, Some(missing));
        }
        Error::ObjectNotFound(_) => {}
        other => panic!("unexpected {other:?}"),
    }
}

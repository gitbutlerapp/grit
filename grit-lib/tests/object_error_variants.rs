//! Typed object-store errors: damaged fixtures from system git, variant assertions.

#[allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::error::Error;
use grit_lib::objects::ObjectError;
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
        ObjectError::CorruptLooseObject { oid: got, .. } => {
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

    use grit_lib::error::Error;
    let err = read_pack_index(&idx).expect_err("bad idx checksum");
    match err {
        Error::Object(ObjectError::PackIndexChecksumMismatch { .. })
        | Error::Object(ObjectError::PackIndexChecksumMismatch { .. }) => {}
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
        ObjectError::ObjectNotFound { oid, .. } => {
            assert_eq!(oid, Some(missing));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn thin_pack_unresolved_deltas_without_bases() {
    use grit_lib::error::Error;
    use grit_lib::objects::odb::store::MemoryStore;
    use grit_lib::objects::unpack_objects::{ingest_pack_into, pack_is_thin, UnpackOptions};
    use grit_lib::objects::HashAlgo;
    use std::io::{Cursor, Write};
    use std::process::Command;

    fn pack_objects(dir: &std::path::Path, revs: &str, extra: &[&str]) -> Vec<u8> {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .args(["pack-objects", "--revs", "--stdout"])
            .args(extra);
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("pack-objects");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(revs.as_bytes())
            .expect("write revs");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "git pack-objects failed");
        out.stdout
    }

    let upstream = init_repo();
    let base = b"0".repeat(200_000);
    fs::write(upstream.path().join("big.txt"), &base).expect("write");
    upstream.git(&["add", "big.txt"]);
    upstream.git(&["commit", "-m", "base"]);
    let mut tip_blob = base.to_vec();
    tip_blob.extend_from_slice(b"\nthin-pack tail\n");
    fs::write(upstream.path().join("big.txt"), &tip_blob).expect("write");
    upstream.git(&["commit", "-am", "tip"]);
    let thin = pack_objects(upstream.path(), "HEAD\n^HEAD~\n", &["--thin"]);
    assert!(
        pack_is_thin(&thin, HashAlgo::Sha1),
        "git pack-objects --thin should produce a thin pack"
    );
    let target = MemoryStore::new(HashAlgo::Sha1);
    let err = ingest_pack_into(
        &mut Cursor::new(&thin),
        &target,
        &target,
        &UnpackOptions::default(),
    )
    .expect_err("thin pack without bases");
    let object_err = match err {
        Error::Object(inner) => inner,
        Error::Object(ObjectError::UnresolvedDeltas { count }) => ObjectError::UnresolvedDeltas { count },
        other => panic!("expected unresolved deltas, got {other:?}"),
    };
    match object_err {
        ObjectError::UnresolvedDeltas { count } => assert!(count > 0),
        other => panic!("expected unresolved deltas, got {other:?}"),
    }
}

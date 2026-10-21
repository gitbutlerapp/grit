//! [`Odb::read_into`] matches [`Odb::read`] across loose, packed, delta, alternates, and promisor layouts.

use std::collections::HashSet;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use grit_lib::objects::odb::store::{ObjectStore, PackFilter, PackedObjects};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack_store::PackStore;
use grit_lib::objects::{ObjectId, ObjectKind, ObjectRef};
use grit_test_support::objects::{git_supports_sha256, HashAlgo as FixtureAlgo, RepoFixture};

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_read_into_matches_read(odb: &Odb, oid: &ObjectId) {
    let whole = odb.read(oid).expect("read");
    let mut buf = Vec::new();
    let kind = odb.read_into(oid, &mut buf).expect("read_into");
    assert_eq!(kind, whole.kind);
    assert_eq!(buf, whole.data);
    assert_eq!(ObjectRef::from_buf(kind, &buf), whole.as_ref());
}

fn assert_buffer_reuse(odb: &Odb, oids: &[ObjectId]) {
    let mut buf = Vec::with_capacity(1024);
    let cap_before = buf.capacity();
    for oid in oids {
        let _ = odb.read_into(oid, &mut buf).expect("read_into");
        assert!(
            buf.capacity() >= cap_before,
            "buffer capacity must not shrink across reads"
        );
    }
}

fn deep_delta_git_objects() -> (tempfile::TempDir, PathBuf, ObjectId) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    git_in(dir.path(), &["config", "user.name", "T"]);
    let mut prev = String::new();
    for i in 0..64 {
        let content = format!("layer-{i}-{prev}");
        fs::write(dir.path().join("blob.txt"), &content).expect("write");
        git_in(dir.path(), &["add", "blob.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        prev = content;
    }
    git_in(dir.path(), &["repack", "-adf"]);
    let tip_hex = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("rev-parse");
    let tip = ObjectId::from_hex(String::from_utf8_lossy(&tip_hex.stdout).trim()).expect("tip oid");
    let objects = dir.path().join(".git/objects");
    (dir, objects, tip)
}

#[test]
fn odb_read_into_matches_read_loose_and_packed() {
    let (_dir, objects, tip) = deep_delta_git_objects();
    let odb = Odb::new(&objects);
    assert_read_into_matches_read(&odb, &tip);

    let mut all = Vec::new();
    odb.for_each_object(&mut |oid| {
        all.push(*oid);
        ControlFlow::Continue(())
    })
    .expect("for_each");
    for oid in &all {
        assert_read_into_matches_read(&odb, oid);
    }
    assert_buffer_reuse(&odb, &all);

    let packed = PackedObjects::new(Arc::new(PackStore::new(objects.clone())));
    for oid in all.iter().take(20) {
        let whole = packed.read(oid).expect("packed read").expect("hit");
        let mut buf = Vec::new();
        let kind = packed
            .read_into(oid, &mut buf)
            .expect("packed read_into")
            .expect("hit");
        assert_eq!(kind, whole.kind);
        assert_eq!(buf, whole.data);
    }
}

#[test]
fn odb_read_into_matches_read_alternates() {
    let layout = tempfile::TempDir::new().expect("layout");
    let alt_objects = layout.path().join("alt/objects");
    fs::create_dir_all(&alt_objects).expect("alt");
    let alt_odb = Odb::new(&alt_objects);
    let alt_oid = alt_odb
        .write(ObjectKind::Blob, b"via-alternate")
        .expect("write alt");

    git_in(layout.path(), &["init", "-q", "--bare", "primary.git"]);
    let primary_objects = layout.path().join("primary.git/objects");
    let alt_abs = fs::canonicalize(&alt_objects).expect("canonical");
    fs::write(
        primary_objects.join("info/alternates"),
        format!("{}\n", alt_abs.display()),
    )
    .expect("alternates");
    let odb = Odb::new(&primary_objects);
    assert_read_into_matches_read(&odb, &alt_oid);
}

#[test]
fn odb_read_into_matches_read_sha256() {
    if !git_supports_sha256() {
        return;
    }
    let fixture = RepoFixture::init(FixtureAlgo::Sha256).expect("init sha256");
    fs::write(fixture.path().join("f"), b"x").expect("write");
    assert!(fixture.git(&["add", "f"]).ok, "add");
    assert!(fixture.git(&["commit", "-m", "c"]).ok, "commit");
    assert!(fixture.git(&["repack", "-adf"]).ok, "repack");
    let odb = Odb::new(&fixture.path().join(".git/objects"));
    let mut oids = Vec::new();
    odb.for_each_object(&mut |oid| {
        oids.push(*oid);
        ControlFlow::Continue(())
    })
    .expect("for_each");
    for oid in oids {
        assert_read_into_matches_read(&odb, &oid);
    }
}

#[test]
fn odb_read_into_promisor_missing_is_object_not_found() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    fs::write(dir.path().join("f"), b"x").expect("write");
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "c"]);
    git_in(dir.path(), &["repack", "-adf"]);
    let objects = dir.path().join(".git/objects");
    let packed = PackedObjects::new(Arc::new(PackStore::new(objects.clone())));
    let miss = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("miss");
    assert!(packed.read(&miss).expect("read").is_none());
    let mut buf = Vec::new();
    assert!(packed
        .read_into(&miss, &mut buf)
        .expect("read_into")
        .is_none());
    assert!(buf.is_empty());

    let pack_dir = objects.join("pack");
    for entry in fs::read_dir(&pack_dir).expect("pack dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "pack") {
            fs::write(path.with_extension("promisor"), b"").expect("promisor");
        }
    }
    let mut seen = HashSet::new();
    packed
        .for_each_object(&mut |oid| {
            seen.insert(*oid);
            ControlFlow::Continue(())
        })
        .expect("for_each");
    assert!(!seen.is_empty());
    assert!(!packed
        .contains_filtered(&miss, PackFilter::default())
        .expect("contains"));
}

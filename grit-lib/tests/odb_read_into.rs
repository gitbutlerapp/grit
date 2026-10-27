//! [`Odb::read_into`] matches [`Odb::read`] across loose, packed, delta, alternates, and promisor layouts.

use std::collections::HashSet;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use grit_lib::objects::odb::store::{ObjectStore, PackFilter, PackedObjects};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::{
    clear_pack_cache, read_object_from_pack, read_object_from_pack_into, read_pack_index,
};
use grit_lib::objects::pack_store::PackStore;
use grit_lib::objects::{ObjectId, ObjectKind, ObjectRef};
use grit_test_support::objects::{git_supports_sha256, HashAlgo as FixtureAlgo, RepoFixture};

const SENTINEL_CAPACITY: usize = 1024 * 1024;

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

fn assert_read_into_preserves_caller_buffer(odb: &Odb, oid: &ObjectId) {
    let whole = odb.read(oid).expect("read");
    let mut buf = Vec::with_capacity(SENTINEL_CAPACITY);
    let ptr = buf.as_ptr();
    let cap = buf.capacity();
    assert!(cap >= SENTINEL_CAPACITY);
    let kind = odb.read_into(oid, &mut buf).expect("read_into");
    assert_eq!(kind, whole.kind);
    assert_eq!(buf, whole.data);
    assert_eq!(buf.capacity(), cap);
    assert_eq!(buf.as_ptr(), ptr);
}

fn deep_delta_git_objects() -> (tempfile::TempDir, PathBuf, ObjectId, Vec<ObjectId>) {
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
    let mut all = Vec::new();
    let odb_probe = Odb::new(&objects);
    odb_probe
        .for_each_object(&mut |oid| {
            all.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("for_each");
    (dir, objects, tip, all)
}

fn pack_has_delta_objects(objects: &Path) -> bool {
    let pack_dir = objects.join("pack");
    let Ok(rd) = fs::read_dir(&pack_dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "pack") {
            // Run inside the repository so `git` uses its hash algorithm.
            let out = Command::new("git")
                .current_dir(objects)
                .args(["verify-pack", "-v"])
                .arg(&path)
                .output()
                .expect("verify-pack");
            if !out.status.success() {
                continue;
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if line.split_whitespace().count() >= 7 {
                    return true;
                }
            }
        }
    }
    false
}

fn deepest_deltified_oid(objects: &Path) -> ObjectId {
    let pack_dir = objects.join("pack");
    let pack = fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "pack"))
        .expect("pack");
    let out = Command::new("git")
        .current_dir(objects)
        .args(["verify-pack", "-v"])
        .arg(&pack)
        .output()
        .expect("verify-pack");
    assert!(out.status.success());
    let mut best: Option<(u64, ObjectId)> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 7 {
            let depth: u64 = parts[5].parse().expect("depth");
            let oid = ObjectId::from_hex(parts[0]).expect("oid");
            if best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, oid));
            }
        }
    }
    best.expect("delta oid").1
}

fn exercise_repeated_delta_reads(objects: &Path, delta_oid: &ObjectId, interleave: &[ObjectId]) {
    let idx_path = fs::read_dir(objects.join("pack"))
        .expect("pack")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "idx"))
        .expect("idx");
    let idx = read_pack_index(&idx_path).expect("idx");
    let odb = Odb::new(objects);

    let expected = read_object_from_pack(&idx, delta_oid).expect("read baseline");

    let mut buf = Vec::with_capacity(SENTINEL_CAPACITY);
    let ptr = buf.as_ptr();
    let cap = buf.capacity();

    for round in 0..3 {
        if round % 2 == 0 {
            clear_pack_cache();
        }
        for (i, oid) in interleave.iter().enumerate().take(3) {
            let mut scratch = Vec::new();
            let _ = odb.read_into(oid, &mut scratch).expect("interleave odb");
            if i == 0 {
                let _ =
                    read_object_from_pack_into(&idx, oid, &mut scratch).expect("interleave pack");
            }
        }
        let kind = read_object_from_pack_into(&idx, delta_oid, &mut buf).expect("pack into");
        assert_eq!(kind, expected.kind);
        assert_eq!(buf, expected.data, "pack into round {round}");
        assert_eq!(buf.capacity(), cap);
        assert_eq!(buf.as_ptr(), ptr);

        let kind = odb.read_into(delta_oid, &mut buf).expect("odb into");
        assert_eq!(kind, expected.kind);
        assert_eq!(buf, expected.data, "odb into round {round}");
        assert_eq!(buf.capacity(), cap);
        assert_eq!(buf.as_ptr(), ptr);
    }
}

#[test]
fn odb_read_into_matches_read_loose_sha1() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    fs::write(dir.path().join("f"), b"loose-only").expect("write");
    git_in(dir.path(), &["add", "f"]);
    git_in(dir.path(), &["commit", "-q", "-m", "c"]);
    let odb = Odb::new(&dir.path().join(".git/objects"));
    let mut oids = Vec::new();
    odb.for_each_object(&mut |oid| {
        oids.push(*oid);
        ControlFlow::Continue(())
    })
    .expect("for_each");
    for oid in &oids {
        assert_read_into_matches_read(&odb, oid);
        assert_read_into_preserves_caller_buffer(&odb, oid);
    }
}

#[test]
fn odb_read_into_repeated_deep_delta_sha1() {
    let (_dir, objects, _tip, all) = deep_delta_git_objects();
    assert!(pack_has_delta_objects(&objects));
    let delta_oid = deepest_deltified_oid(&objects);
    exercise_repeated_delta_reads(&objects, &delta_oid, &all);
}

#[test]
fn odb_read_into_repeated_deep_delta_sha256() {
    if !git_supports_sha256() {
        return;
    }
    let dir = tempfile::TempDir::new().expect("tempdir");
    git_in(
        dir.path(),
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    let mut prev = String::new();
    for i in 0..64 {
        let content = format!("layer-{i}-{prev}");
        fs::write(dir.path().join("blob.txt"), &content).expect("write");
        git_in(dir.path(), &["add", "blob.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        prev = content;
    }
    git_in(dir.path(), &["repack", "-adf"]);
    let objects = dir.path().join(".git/objects");
    if !pack_has_delta_objects(&objects) {
        eprintln!("skipping: system git did not produce delta objects for sha256 repack");
        return;
    }
    let delta_oid = deepest_deltified_oid(&objects);
    let mut all = Vec::new();
    Odb::new(&objects)
        .for_each_object(&mut |oid| {
            all.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("for_each");
    exercise_repeated_delta_reads(&objects, &delta_oid, &all);
}

#[test]
fn odb_read_into_matches_read_packed_delta_chain() {
    let (_dir, objects, tip, _all) = deep_delta_git_objects();
    assert!(pack_has_delta_objects(&objects));
    let odb = Odb::new(&objects);
    assert_read_into_matches_read(&odb, &tip);
    assert_read_into_preserves_caller_buffer(&odb, &tip);

    let idx_path = fs::read_dir(objects.join("pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|ext| ext == "idx"))
        .expect("pack idx");
    let idx = read_pack_index(&idx_path).expect("read idx");
    let mut buf = Vec::with_capacity(SENTINEL_CAPACITY);
    let ptr = buf.as_ptr();
    let cap = buf.capacity();
    read_object_from_pack_into(&idx, &tip, &mut buf).expect("read_into via pack API");
    assert_eq!(buf.capacity(), cap);
    assert_eq!(buf.as_ptr(), ptr);
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
    assert_read_into_preserves_caller_buffer(&odb, &alt_oid);
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
    for oid in &oids {
        assert_read_into_matches_read(&odb, &oid);
        assert_read_into_preserves_caller_buffer(&odb, &oid);
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

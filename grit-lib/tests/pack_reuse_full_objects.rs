//! Integration tests for verbatim reuse of full packed objects in [`build_pack`].

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pack::{
    packed_full_object_slice, read_packed_delta_dependency, verify_pack_and_collect,
    PackedDeltaDependency, PackedType, VerifyObjectRecord,
};
use grit_lib::transfer::{build_pack, PackBuildOptions};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn unique_tmp(tag: &str) -> tempfile::TempDir {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let base =
        std::env::temp_dir().join(format!("grit-pack-reuse-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create temp dir");
    tempfile::TempDir::new_in(&base).expect("tempdir")
}

fn git_index_pack_dir(pack: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let scratch = tempfile::tempdir().expect("scratch");
    let pack_path = scratch.path().join("input.pack");
    std::fs::write(&pack_path, pack).expect("write pack");
    let out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "input.pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        out.status.success(),
        "git index-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let idx = std::fs::read_dir(scratch.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack idx beside pack");
    (scratch, idx)
}

fn init_repo_with_blob(dir: &Path, body: &[u8]) -> (ObjectId, ObjectId) {
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("blob.bin"), body).unwrap();
    git(dir, &["add", "blob.bin"]);
    git(dir, &["commit", "-qm", "c1"]);
    git(dir, &["repack", "-a", "-d"]);
    let tip = ObjectId::from_hex(git(dir, &["rev-parse", "HEAD"]).trim()).expect("tip");
    let blob = ObjectId::from_hex(git(dir, &["rev-parse", "HEAD:blob.bin"]).trim()).expect("blob");
    (tip, blob)
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir)
}

fn verbatim_reused_full_offsets(
    objects: &Path,
    pack_bytes: &[u8],
    records: &[VerifyObjectRecord],
) -> HashSet<u64> {
    let mut out = HashSet::new();
    for rec in records {
        if matches!(rec.packed_type, PackedType::OfsDelta | PackedType::RefDelta) {
            continue;
        }
        let Ok(oid) = ObjectId::from_bytes(rec.oid.as_slice()) else {
            continue;
        };
        let Ok(Some(source)) = packed_full_object_slice(objects, &oid) else {
            continue;
        };
        let start = rec.offset as usize;
        let end = start.saturating_add(rec.size_in_pack as usize);
        if end <= pack_bytes.len() && pack_bytes.get(start..end) == Some(source.as_slice()) {
            out.insert(rec.offset);
        }
    }
    out
}

#[test]
fn build_pack_reuses_full_objects_and_round_trips() {
    let dir = unique_tmp("roundtrip");
    let body = b"reuse-me payload with enough bytes to pack\n";
    let (tip, blob) = init_repo_with_blob(dir.path(), body);
    let objects = dir.path().join(".git").join("objects");
    assert!(
        packed_full_object_slice(&objects, &blob)
            .expect("slice lookup")
            .is_some(),
        "source pack should expose a reusable slice"
    );

    let source_slice = packed_full_object_slice(&objects, &blob)
        .expect("lookup")
        .expect("source slice");
    let odb = open_odb(dir.path());
    let pack = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("build pack");
    assert!(
        pack.windows(source_slice.len())
            .any(|w| w == source_slice.as_slice()),
        "output pack should contain verbatim reused bytes"
    );
    let (scratch, idx_path) = git_index_pack_dir(&pack);
    verify_pack_and_collect(&idx_path).expect("verify grit pack");

    let pack_on_disk = grit_lib::pack::read_pack_index(&idx_path)
        .expect("read idx")
        .pack_path;
    let ip = Command::new("git")
        .current_dir(scratch.path())
        .args([
            "index-pack",
            "--strict",
            pack_on_disk
                .file_name()
                .unwrap()
                .to_str()
                .expect("utf8 name"),
        ])
        .output()
        .expect("index-pack strict");
    assert!(
        ip.status.success(),
        "git index-pack --strict: {}",
        String::from_utf8_lossy(&ip.stderr)
    );
}

#[test]
fn build_pack_reuse_objects_disabled_still_valid() {
    let dir = unique_tmp("no-reuse");
    let (tip, _blob) = init_repo_with_blob(dir.path(), b"no verbatim reuse\n");
    let odb = open_odb(dir.path());
    let pack = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            reuse_objects: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    let (_scratch, idx_path) = git_index_pack_dir(&pack);
    verify_pack_and_collect(&idx_path).expect("verify pack");
}

#[test]
fn build_pack_ofs_delta_resolves_reused_base() {
    let dir = unique_tmp("ofs-base");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!("seed {i:04}\n"));
    }
    for rev in 0..40 {
        body.push_str(&format!("rev-{rev}\n"));
        std::fs::write(dir.path().join("chain.txt"), body.as_bytes()).unwrap();
        git(dir.path(), &["add", "chain.txt"]);
        git(dir.path(), &["commit", "-qm", &format!("c{rev}")]);
    }
    git(dir.path(), &["repack", "-a", "-d"]);
    let tip = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).trim()).expect("tip");
    let objects = dir.path().join(".git").join("objects");
    let odb = open_odb(dir.path());
    let pack = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            window: 10,
            reuse_deltas: true,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    let (_scratch, idx_path) = git_index_pack_dir(&pack);
    let records = verify_pack_and_collect(&idx_path).expect("records");
    let pack_bytes = &pack;

    let reused_full = verbatim_reused_full_offsets(&objects, pack_bytes, &records);
    assert!(
        !reused_full.is_empty(),
        "fixture must emit at least one verbatim reused full object"
    );

    let ofs_deltas: Vec<_> = records
        .iter()
        .filter(|r| r.packed_type == PackedType::OfsDelta)
        .collect();
    assert!(
        !ofs_deltas.is_empty(),
        "expected OFS_DELTA entries with use_ofs_delta (REF_DELTA fallback means reused-base registration failed)"
    );

    let mut ofs_bases_reused_full = false;
    for rec in &ofs_deltas {
        let dep = read_packed_delta_dependency(pack_bytes, rec.offset)
            .expect("parse delta dependency")
            .expect("delta header");
        if let PackedDeltaDependency::OfsBase { base_offset } = dep {
            if reused_full.contains(&base_offset) {
                ofs_bases_reused_full = true;
                break;
            }
        }
    }
    assert!(
        ofs_bases_reused_full,
        "at least one OFS_DELTA must reference the pack offset of a verbatim reused full object"
    );

    let ref_delta_with_reusable_base = records
        .iter()
        .filter(|r| r.packed_type == PackedType::RefDelta)
        .filter(|r| {
            r.base_oid.as_ref().is_some_and(|raw| {
                ObjectId::from_bytes(raw)
                    .ok()
                    .and_then(|oid| packed_full_object_slice(&objects, &oid).ok().flatten())
                    .is_some()
            })
        })
        .count();
    assert_eq!(
        ref_delta_with_reusable_base, 0,
        "delta bases with available verbatim pack slices must not fall back to REF_DELTA"
    );
}

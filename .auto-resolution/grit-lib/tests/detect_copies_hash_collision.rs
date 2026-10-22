//! Copy detection accepts blobs whose payload alone triggers sha1dc, as Git does.

use std::path::Path;

use grit_lib::config::ConfigSet;
use grit_lib::diff::{detect_copies_with_policy, zero_oid, DiffEntry, DiffStatus};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;

fn shattered_prefix(which: u8) -> Vec<u8> {
    let name = format!("shattered-{which}-prefix.bin");
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .expect("fixture")
}

#[test]
fn detect_copies_accepts_shattered_modified_worktree_blob() {
    let td = tempfile::tempdir().expect("tempdir");
    let wt = td.path();
    let objects = wt.join("objects");
    std::fs::create_dir_all(&objects).expect("objects dir");

    let odb = Odb::new(&objects);
    let src_oid = odb
        .write(ObjectKind::Blob, b"copy source payload\n")
        .expect("source blob");

    std::fs::write(wt.join("source.txt"), b"copy source payload\n").expect("source file");
    std::fs::write(wt.join("target.txt"), shattered_prefix(1)).expect("shattered file");

    let source_entry = DiffEntry {
        status: DiffStatus::Modified,
        old_path: Some("source.txt".into()),
        new_path: Some("source.txt".into()),
        old_mode: "100644".into(),
        new_mode: "100644".into(),
        old_oid: src_oid,
        new_oid: src_oid,
        score: None,
    };
    let shattered_entry = DiffEntry {
        status: DiffStatus::Modified,
        old_path: Some("target.txt".into()),
        new_path: Some("target.txt".into()),
        old_mode: "100644".into(),
        new_mode: "100644".into(),
        old_oid: ObjectId::from_hex("0000000000000000000000000000000000000000").expect("zero"),
        new_oid: zero_oid(),
        score: None,
    };

    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let result = detect_copies_with_policy(
        &odb,
        Some(wt),
        vec![source_entry, shattered_entry],
        50,
        false,
        &[],
        &config,
        &policy,
    )
    .expect("copy detection accepts SHAttered payload as a blob");
    assert_eq!(result.len(), 2);
    assert!(result
        .iter()
        .all(|entry| entry.status == DiffStatus::Modified));
}

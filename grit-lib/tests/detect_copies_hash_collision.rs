//! Copy detection accepts blobs whose payload alone triggers sha1dc, as Git does.

use grit_lib::objects::FileMode;
use std::path::Path;

use grit_lib::diff::{zero_oid, CopyDetection, DiffEntry, DiffStatus, TreeDiffOptions};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

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
    let (repo, _) = Repository::init(wt, &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let src_oid = odb
        .write(ObjectKind::Blob, b"copy source payload\n")
        .expect("source blob");

    std::fs::write(wt.join("source.txt"), b"copy source payload\n").expect("source file");
    std::fs::write(wt.join("target.txt"), shattered_prefix(1)).expect("shattered file");

    let source_entry = DiffEntry::new(
        DiffStatus::Modified,
        Some("source.txt".into()),
        Some("source.txt".into()),
        FileMode::REGULAR,
        FileMode::REGULAR,
        src_oid,
        src_oid,
    );
    let shattered_entry = DiffEntry::new(
        DiffStatus::Modified,
        Some("target.txt".into()),
        Some("target.txt".into()),
        FileMode::REGULAR,
        FileMode::REGULAR,
        ObjectId::from_hex("0000000000000000000000000000000000000000").expect("zero"),
        zero_oid(),
    );

    let options = TreeDiffOptions::default().with_copies(CopyDetection::Similarity(50));
    let result = repo
        .diff()
        .detect_renames(vec![source_entry, shattered_entry], None, &options)
        .expect("copy detection accepts SHAttered payload as a blob")
        .entries;
    assert_eq!(result.len(), 2);
    assert!(result
        .iter()
        .all(|entry| entry.status == DiffStatus::Modified));
}

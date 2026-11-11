//! Coverage for the `IndexEntry` and `TreeEntry` constructors, setters and typed accessors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::error::{Error, ObjectError};
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE};
use grit_lib::objects::{
    parse_tree, serialize_tree, FileMode, ObjectId, ObjectKind, TreeEntry, TreeEntryError,
};
use grit_lib::repo::{InitOptions, Repository};

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn index_entry_new_is_stage_zero_with_zeroed_stat_data() {
    let entry = IndexEntry::new("dir/file.txt", oid(1), FileMode::EXECUTABLE);
    assert_eq!(entry.path, b"dir/file.txt");
    assert_eq!(entry.oid, oid(1));
    assert_eq!(entry.mode, MODE_EXECUTABLE);
    assert_eq!(entry.stage(), 0);
    assert!(entry.stat_cache_unset());
    assert_eq!(
        (entry.ctime_sec, entry.dev, entry.ino, entry.uid, entry.gid),
        (0, 0, 0, 0, 0)
    );
    assert!(!entry.assume_unchanged());
    assert!(!entry.skip_worktree());
    assert!(!entry.intent_to_add());
    assert!(!entry.fsmonitor_valid());
}

#[test]
fn index_entry_setters_cover_stage_and_stat_data() {
    let entry = IndexEntry::new(b"a".to_vec(), oid(2), FileMode::REGULAR)
        .with_stage(3)
        .with_ctime(10, 11)
        .with_mtime(12, 13)
        .with_size(14);
    assert_eq!(entry.stage(), 3);
    assert_eq!((entry.ctime_sec, entry.ctime_nsec), (10, 11));
    assert_eq!((entry.mtime_sec, entry.mtime_nsec), (12, 13));
    assert_eq!(entry.size, 14);
    assert!(!entry.stat_cache_unset());

    let mut entry = entry;
    entry.set_stage(1);
    assert_eq!(entry.stage(), 1);
    // Only the low two bits count.
    entry.set_stage(6);
    assert_eq!(entry.stage(), 2);
}

#[test]
fn index_entry_equality_ignores_name_length_bits() {
    let original = IndexEntry::new("same", oid(3), FileMode::REGULAR);
    let mut renamed = IndexEntry::new("a-much-longer-name", oid(3), FileMode::REGULAR);
    renamed.path = b"same".to_vec();
    assert_eq!(original, renamed, "stale length bits must not matter");
    assert_ne!(original, original.clone().with_stage(2));
    let mut assumed = original.clone();
    assumed.set_assume_unchanged(true);
    assert_ne!(original, assumed);
}

#[test]
fn index_entries_round_trip_through_an_index_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let long_path = format!("{}/leaf", "d".repeat(0x1100));
    let mut index = Index::new();
    let entries = [
        IndexEntry::new("conflict", oid(4), FileMode::REGULAR).with_stage(1),
        IndexEntry::new("conflict", oid(5), FileMode::REGULAR).with_stage(2),
        IndexEntry::new(long_path.as_bytes(), oid(6), FileMode::SYMLINK).with_size(9),
        IndexEntry::new("plain", oid(7), FileMode::EXECUTABLE).with_mtime(100, 5),
    ];
    for entry in entries.iter().cloned() {
        index.add_or_replace(entry);
    }
    let path = dir.path().join("index");
    index.write(&path).expect("write index");
    let read = Index::load(&path).expect("load index");
    let mut expected = entries.to_vec();
    expected.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage().cmp(&b.stage())));
    assert_eq!(read.entries(), expected.as_slice());
}

#[test]
fn tree_entry_new_round_trips_through_parse_tree() {
    let entries = vec![
        TreeEntry::new("a.txt", FileMode::REGULAR, oid(8)),
        TreeEntry::new(b"dir".to_vec(), FileMode::TREE, oid(9)),
    ];
    assert!(!entries[0].is_tree());
    assert!(entries[1].is_tree());
    assert_eq!(entries[1].mode_str(), "40000");
    let parsed = parse_tree(&serialize_tree(&entries)).expect("parse");
    assert_eq!(parsed, entries);
}

#[test]
fn tree_entry_mode_keeps_legacy_modes_that_write_tree_rejects() {
    // `100664` is a legacy mode old Git wrote; fsck reports it, so FileMode must hold it.
    let legacy = TreeEntry::new("old", FileMode::new(0o100664), oid(10));
    let parsed = parse_tree(&serialize_tree(std::slice::from_ref(&legacy))).expect("parse");
    assert_eq!(parsed[0].mode.bits(), 0o100664);
    assert_eq!(parsed[0].mode_str(), "100664");

    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) =
        Repository::init(dir.path(), &InitOptions::new().with_bare(true)).expect("init");
    let blob = repo.objects().write(ObjectKind::Blob, b"x").expect("blob");
    let err = repo
        .objects()
        .write_tree([TreeEntry::new("old", FileMode::new(0o100664), blob)])
        .expect_err("legacy mode rejected");
    assert!(
        matches!(
            &err,
            Error::Object(ObjectError::InvalidTreeEntry(TreeEntryError::InvalidMode { mode, .. }))
                if mode.bits() == 0o100664
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "tree entry 'old' has unsupported mode 100664"
    );
}

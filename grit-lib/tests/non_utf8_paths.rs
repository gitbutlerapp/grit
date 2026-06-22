//! Phase 1 regression: a diff over a tree whose entry name is **not** valid
//! UTF-8 must preserve the path bytes exactly, rather than mangling them with
//! `String::from_utf8_lossy` (U+FFFD). See `docs/non-utf8-paths-design.md`.

use grit_lib::diff::{diff_trees, DiffStatus};
use grit_lib::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::odb::Odb;

/// A filename with a stray Latin-1 byte (`0xE9`, 'é') and a raw `0xFF` — both
/// illegal in UTF-8, both legal in a Git tree on Unix.
const RAW_NAME: &[u8] = b"caf\xe9-\xff.txt";

fn write_blob(odb: &Odb, contents: &[u8]) -> ObjectId {
    odb.write(ObjectKind::Blob, contents).unwrap()
}

fn write_tree(odb: &Odb, name: &[u8], blob: ObjectId) -> ObjectId {
    let entries = vec![TreeEntry {
        mode: 0o100644,
        name: name.to_vec(),
        oid: blob,
    }];
    odb.write(ObjectKind::Tree, &serialize_tree(&entries)).unwrap()
}

#[test]
fn added_entry_preserves_non_utf8_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let odb = Odb::new(tmp.path());

    let blob = write_blob(&odb, b"hello\n");
    let tree = write_tree(&odb, RAW_NAME, blob);

    // Empty tree -> our tree: the file is Added.
    let entries = diff_trees(&odb, None, Some(&tree), "").unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.status, DiffStatus::Added);

    let path = e.new_path.as_ref().expect("added entry has a new_path");
    // The bytes must survive verbatim — no U+FFFD substitution.
    assert_eq!(path.as_bytes(), RAW_NAME);
    assert!(path.to_str().is_none(), "name is intentionally non-UTF-8");
}

#[test]
fn modified_entry_preserves_non_utf8_bytes_on_both_sides() {
    let tmp = tempfile::tempdir().unwrap();
    let odb = Odb::new(tmp.path());

    let old_blob = write_blob(&odb, b"one\n");
    let new_blob = write_blob(&odb, b"two\n");
    let old_tree = write_tree(&odb, RAW_NAME, old_blob);
    let new_tree = write_tree(&odb, RAW_NAME, new_blob);

    let entries = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "").unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.status, DiffStatus::Modified);
    assert_eq!(e.old_path.as_ref().unwrap().as_bytes(), RAW_NAME);
    assert_eq!(e.new_path.as_ref().unwrap().as_bytes(), RAW_NAME);
}

#[test]
fn nested_non_utf8_directory_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let odb = Odb::new(tmp.path());

    // A subtree whose *directory* name is non-UTF-8, holding a normal file.
    let blob = write_blob(&odb, b"x\n");
    let inner = vec![TreeEntry {
        mode: 0o100644,
        name: b"file.txt".to_vec(),
        oid: blob,
    }];
    let inner_oid = odb.write(ObjectKind::Tree, &serialize_tree(&inner)).unwrap();
    let dir_name: &[u8] = b"d\xe9r";
    let outer = vec![TreeEntry {
        mode: 0o040000,
        name: dir_name.to_vec(),
        oid: inner_oid,
    }];
    let outer_oid = odb.write(ObjectKind::Tree, &serialize_tree(&outer)).unwrap();

    let entries = diff_trees(&odb, None, Some(&outer_oid), "").unwrap();
    assert_eq!(entries.len(), 1);
    // The non-UTF-8 directory prefix must be carried through the recursion.
    let mut expected = dir_name.to_vec();
    expected.extend_from_slice(b"/file.txt");
    assert_eq!(entries[0].new_path.as_ref().unwrap().as_bytes(), &expected[..]);
}

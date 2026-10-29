//! Path-level tree comparison and editing for grafts, slices and merges.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::objects::{parse_tree, serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;

pub(crate) const TREE_MODE: u32 = 0o040000;

/// An entry at a path: its mode and object.
pub(crate) type Entry = (u32, ObjectId);

/// A path and what to set there (`None` removes it).
pub(crate) type Edit = (Vec<u8>, Option<Entry>);

/// One path whose entry differs between two trees. Trees themselves are never
/// reported; a directory that appears or disappears shows up as its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    /// Slash-separated path.
    pub(crate) path: Vec<u8>,
    /// The entry before, if any.
    pub(crate) old: Option<Entry>,
    /// The entry after, if any.
    pub(crate) new: Option<Entry>,
}

fn entries(odb: &Odb, tree: Option<ObjectId>) -> Result<Vec<TreeEntry>> {
    match tree {
        Some(t) => parse_tree(&odb.read(&t)?.data),
        None => Ok(Vec::new()),
    }
}

/// Every non-tree path that differs between `old` and `new`.
pub(crate) fn diff(odb: &Odb, old: Option<ObjectId>, new: Option<ObjectId>) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    diff_into(odb, old, new, b"", &mut out)?;
    Ok(out)
}

fn diff_into(
    odb: &Odb,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    prefix: &[u8],
    out: &mut Vec<Change>,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    let mut by_name: BTreeMap<Vec<u8>, (Option<Entry>, Option<Entry>)> = BTreeMap::new();
    for e in entries(odb, old)? {
        by_name.entry(e.name).or_default().0 = Some((e.mode, e.oid));
    }
    for e in entries(odb, new)? {
        by_name.entry(e.name).or_default().1 = Some((e.mode, e.oid));
    }
    for (name, (a, b)) in by_name {
        if a == b {
            continue;
        }
        let path = if prefix.is_empty() {
            name
        } else {
            [prefix, b"/", &name].concat()
        };
        let a_tree = a.filter(|e| e.0 == TREE_MODE).map(|e| e.1);
        let b_tree = b.filter(|e| e.0 == TREE_MODE).map(|e| e.1);
        if a_tree.is_some() || b_tree.is_some() {
            diff_into(odb, a_tree, b_tree, &path, out)?;
        }
        let a_file = a.filter(|e| e.0 != TREE_MODE);
        let b_file = b.filter(|e| e.0 != TREE_MODE);
        if a_file != b_file {
            out.push(Change {
                path,
                old: a_file,
                new: b_file,
            });
        }
    }
    Ok(())
}

/// The entry at `path` under `tree`.
pub(crate) fn lookup(odb: &Odb, tree: ObjectId, path: &[u8]) -> Result<Option<Entry>> {
    Ok(odb
        .tree_entry_at_path(&tree, path)?
        .map(|e| (e.mode, e.oid)))
}

/// `base` (or an empty tree) with each `(path, entry)` set, or removed when
/// the entry is `None`. Only the trees along edited paths are rewritten;
/// directories left empty disappear.
///
/// Setting a path replaces whatever has its name, so a directory that becomes
/// a file (or a file that becomes a directory) works in one batch: removing a
/// name removes only a file there, and a directory emptied by the batch goes
/// only while it is still a directory.
pub(crate) fn edit(
    odb: &Odb,
    base: Option<ObjectId>,
    edits: &[(Vec<u8>, Option<Entry>)],
) -> Result<ObjectId> {
    let mut grouped: BTreeMap<Vec<u8>, Vec<Edit>> = BTreeMap::new();
    let mut here: Vec<Edit> = Vec::new();
    for (path, entry) in edits {
        match path.iter().position(|b| *b == b'/') {
            Some(i) => grouped
                .entry(path[..i].to_vec())
                .or_default()
                .push((path[i + 1..].to_vec(), *entry)),
            None => here.push((path.clone(), *entry)),
        }
    }
    let mut current: BTreeMap<Vec<u8>, Entry> = entries(odb, base)?
        .into_iter()
        .map(|e| (e.name, (e.mode, e.oid)))
        .collect();
    for (name, entry) in here {
        match entry {
            Some(e) => {
                current.insert(name, e);
            }
            None => {
                if current.get(&name).is_some_and(|e| e.0 != TREE_MODE) {
                    current.remove(&name);
                }
            }
        }
    }
    for (dir, sub) in grouped {
        let sub_base = current.get(&dir).filter(|e| e.0 == TREE_MODE).map(|e| e.1);
        let tree = edit(odb, sub_base, &sub)?;
        if parse_tree(&odb.read(&tree)?.data)?.is_empty() {
            // A file set at this name in the same batch stays.
            if sub_base.is_some() {
                current.remove(&dir);
            }
        } else {
            current.insert(dir, (TREE_MODE, tree));
        }
    }
    let mut list: Vec<TreeEntry> = current
        .into_iter()
        .map(|(name, (mode, oid))| TreeEntry { mode, name, oid })
        .collect();
    list.sort_by(|a, b| tree_entry_cmp(&a.name, a.mode == TREE_MODE, &b.name, b.mode == TREE_MODE));
    odb.write(ObjectKind::Tree, &serialize_tree(&list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{InitOptions, Repository};

    fn blob(odb: &Odb, text: &str) -> Entry {
        match odb.write(ObjectKind::Blob, text.as_bytes()) {
            Ok(oid) => (0o100644, oid),
            Err(e) => panic!("write blob: {e}"),
        }
    }

    #[test]
    fn edits_and_diffs_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let (a, b, c) = (blob(odb, "a"), blob(odb, "b"), blob(odb, "c"));
        let base = edit(
            odb,
            None,
            &[
                (b"top".to_vec(), Some(a)),
                (b"src/x".to_vec(), Some(b)),
                (b"src/deep/y".to_vec(), Some(c)),
            ],
        )
        .expect("base");
        assert_eq!(lookup(odb, base, b"src/deep/y").expect("lookup"), Some(c));
        let next = edit(
            odb,
            Some(base),
            &[(b"src/deep/y".to_vec(), None), (b"src/x".to_vec(), Some(a))],
        )
        .expect("edit");
        assert_eq!(
            lookup(odb, next, b"src/deep").expect("lookup"),
            None,
            "empty dirs go"
        );
        let changes = diff(odb, Some(base), Some(next)).expect("diff");
        assert_eq!(
            changes,
            vec![
                Change {
                    path: b"src/deep/y".to_vec(),
                    old: Some(c),
                    new: None
                },
                Change {
                    path: b"src/x".to_vec(),
                    old: Some(b),
                    new: Some(a)
                },
            ]
        );
        assert!(diff(odb, Some(next), Some(next)).expect("diff").is_empty());
    }

    #[test]
    fn a_directory_and_a_file_replace_each_other_in_one_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let (x, y, f) = (blob(odb, "x"), blob(odb, "y"), blob(odb, "f"));
        let with_dir = edit(
            odb,
            None,
            &[(b"lib/x".to_vec(), Some(x)), (b"lib/y".to_vec(), Some(y))],
        )
        .expect("tree");
        let with_file = edit(odb, None, &[(b"lib".to_vec(), Some(f))]).expect("tree");

        // A directory emptied while a file is set at its name: the file stays,
        // whatever order the edits come in.
        for edits in [
            vec![
                (b"lib/x".to_vec(), None),
                (b"lib".to_vec(), Some(f)),
                (b"lib/y".to_vec(), None),
            ],
            vec![
                (b"lib".to_vec(), Some(f)),
                (b"lib/x".to_vec(), None),
                (b"lib/y".to_vec(), None),
            ],
        ] {
            assert_eq!(edit(odb, Some(with_dir), &edits).expect("edit"), with_file);
        }
        // A file removed while a directory gets files at its name.
        let back = edit(
            odb,
            Some(with_file),
            &[
                (b"lib/x".to_vec(), Some(x)),
                (b"lib".to_vec(), None),
                (b"lib/y".to_vec(), Some(y)),
            ],
        )
        .expect("edit");
        assert_eq!(back, with_dir);
        // The diff of a replacement, applied, gives the replacement.
        let changes = diff(odb, Some(with_dir), Some(with_file)).expect("diff");
        let edits: Vec<Edit> = changes.into_iter().map(|c| (c.path, c.new)).collect();
        assert_eq!(edit(odb, Some(with_dir), &edits).expect("edit"), with_file);
    }
}

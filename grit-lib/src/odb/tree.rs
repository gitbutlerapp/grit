//! Looking up one path inside a tree object.
//!
//! Callers that need "the entry at `dir/file` in this tree" share [`Odb::tree_entry_at_path`]
//! instead of reading and parsing each tree on the way by hand.

use crate::error::Result;
use crate::objects::{ObjectId, TreeEntry, TreeRefIter};

use super::{Odb, SessionTree};

/// Mode of a subtree entry.
const MODE_TREE: u32 = 0o040000;

impl Odb {
    /// The entry at `path` inside the tree `tree`.
    ///
    /// Each `/`-separated component of `path` is looked up in turn; every component but the last
    /// must name a subtree. The final entry is returned whatever its mode, so callers that want
    /// only files filter out [`TreeEntry::mode`] `0o040000` themselves. Trees on the way are read
    /// through this handle's bounded tree cache, so repeated lookups under the same directories
    /// do not read or decode them again.
    ///
    /// # Parameters
    ///
    /// - `tree`: the root tree to start from.
    /// - `path`: a repository-relative path in tree byte form (no leading or trailing `/`).
    ///
    /// # Returns
    ///
    /// The entry, with its name set to the last component, or `None` when a component is missing
    /// or a non-final component is not a subtree. An empty `path` or one with empty components
    /// names no entry.
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::ObjectNotFound`] when a tree on the way is missing.
    /// - [`crate::error::Error::CorruptObject`] when a tree on the way is not a tree object or
    ///   does not parse.
    /// - Any other error [`Self::read`] reports.
    pub fn tree_entry_at_path(&self, tree: &ObjectId, path: &[u8]) -> Result<Option<TreeEntry>> {
        let mut current = *tree;
        let mut components = path.split(|&b| b == b'/').peekable();
        while let Some(name) = components.next() {
            let Some((mode, oid)) = self.tree_child(&current, name)? else {
                return Ok(None);
            };
            if components.peek().is_none() {
                return Ok(Some(TreeEntry {
                    mode,
                    name: name.to_vec(),
                    oid,
                }));
            }
            if mode != MODE_TREE {
                return Ok(None);
            }
            current = oid;
        }
        Ok(None)
    }

    /// Mode and id of the entry called `name` directly inside `tree`.
    fn tree_child(&self, tree: &ObjectId, name: &[u8]) -> Result<Option<(u32, ObjectId)>> {
        match self.cached_tree_for_walk(tree)? {
            SessionTree::Parsed(entries) => Ok(entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| (e.mode, e.oid))),
            SessionTree::Raw(data) => {
                for entry in TreeRefIter::with_oid_len(&data, self.hash_algo().len()) {
                    let entry = entry?;
                    if entry.name == name {
                        return Ok(Some((entry.mode, entry.oid)));
                    }
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::error::Error;
    use crate::objects::{serialize_tree, ObjectKind};
    use tempfile::TempDir;

    fn write_tree(odb: &Odb, mut entries: Vec<TreeEntry>) -> ObjectId {
        entries.sort_by(|a, b| {
            crate::objects::tree_entry_cmp(
                &a.name,
                a.mode == MODE_TREE,
                &b.name,
                b.mode == MODE_TREE,
            )
        });
        odb.write(ObjectKind::Tree, &serialize_tree(&entries))
            .unwrap()
    }

    fn entry(mode: u32, name: &[u8], oid: ObjectId) -> TreeEntry {
        TreeEntry {
            mode,
            name: name.to_vec(),
            oid,
        }
    }

    /// `file`, `link -> file`, `sub/` (a gitlink), and `dir/{a, nested/b}`.
    fn fixture(odb: &Odb) -> (ObjectId, ObjectId, ObjectId, ObjectId) {
        let blob_a = odb.write(ObjectKind::Blob, b"a\n").unwrap();
        let blob_b = odb.write(ObjectKind::Blob, b"b\n").unwrap();
        let nested = write_tree(odb, vec![entry(0o100755, b"b", blob_b)]);
        let dir = write_tree(
            odb,
            vec![
                entry(0o100644, b"a", blob_a),
                entry(MODE_TREE, b"nested", nested),
            ],
        );
        let gitlink = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let root = write_tree(
            odb,
            vec![
                entry(0o100644, b"file", blob_a),
                entry(0o120000, b"link", blob_b),
                entry(0o160000, b"sub", gitlink),
                entry(MODE_TREE, b"dir", dir),
            ],
        );
        (root, dir, nested, blob_b)
    }

    #[test]
    fn finds_entries_of_every_mode() {
        let tmp = TempDir::new().unwrap();
        let odb = Odb::new(tmp.path());
        let (root, dir, nested, blob_b) = fixture(&odb);

        let found = odb
            .tree_entry_at_path(&root, b"dir/nested/b")
            .unwrap()
            .unwrap();
        assert_eq!(found, entry(0o100755, b"b", blob_b));
        assert_eq!(
            odb.tree_entry_at_path(&root, b"dir").unwrap().unwrap(),
            entry(MODE_TREE, b"dir", dir)
        );
        assert_eq!(
            odb.tree_entry_at_path(&root, b"dir/nested")
                .unwrap()
                .unwrap()
                .oid,
            nested
        );
        assert_eq!(
            odb.tree_entry_at_path(&root, b"link")
                .unwrap()
                .unwrap()
                .mode,
            0o120000
        );
        assert_eq!(
            odb.tree_entry_at_path(&root, b"sub").unwrap().unwrap().mode,
            0o160000
        );
    }

    #[test]
    fn missing_paths_are_none() {
        let tmp = TempDir::new().unwrap();
        let odb = Odb::new(tmp.path());
        let (root, ..) = fixture(&odb);

        for path in [
            &b"absent"[..],
            b"dir/absent",
            b"dir/nested/b/deeper",
            b"file/child",
            b"sub/child",
            b"",
            b"dir/",
            b"/dir",
            b"dir//a",
            b"di",
        ] {
            assert_eq!(
                odb.tree_entry_at_path(&root, path).unwrap(),
                None,
                "{path:?}"
            );
        }
    }

    #[test]
    fn parsed_and_raw_cache_entries_agree() {
        let tmp = TempDir::new().unwrap();
        let odb = Odb::new(tmp.path());
        let (root, ..) = fixture(&odb);
        let raw = odb.tree_entry_at_path(&root, b"dir/a").unwrap();

        let fresh = Odb::new(tmp.path());
        fresh.cached_tree_entries(&root).unwrap();
        assert_eq!(fresh.tree_entry_at_path(&root, b"dir/a").unwrap(), raw);
        assert!(raw.is_some());
    }

    #[test]
    fn non_tree_roots_and_missing_trees_are_errors() {
        let tmp = TempDir::new().unwrap();
        let odb = Odb::new(tmp.path());
        let blob = odb.write(ObjectKind::Blob, b"not a tree").unwrap();
        assert!(matches!(
            odb.tree_entry_at_path(&blob, b"x"),
            Err(Error::CorruptObject(_))
        ));

        let missing = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        assert!(matches!(
            odb.tree_entry_at_path(&missing, b"x"),
            Err(Error::ObjectNotFound(_))
        ));
    }
}

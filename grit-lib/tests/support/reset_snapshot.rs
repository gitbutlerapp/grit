//! Exact file snapshots for reset's preflight failure tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Capture every worktree and git-directory file, including refs, index, and reflogs.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    files
}

/// Check both the file set and exact bytes after a rejected reset.
pub fn assert_unchanged(root: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    let after = snapshot(root);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "repository file set changed"
    );
    for (path, bytes) in before {
        assert_eq!(
            after.get(path).unwrap(),
            bytes,
            "{} changed",
            path.display()
        );
    }
}

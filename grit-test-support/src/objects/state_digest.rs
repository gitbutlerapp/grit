//! Before/after digests of a `.git` directory for read-only (`--dry-run`) checks.

use std::fs;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

/// Stable digest of a `.git` directory tree (objects, refs, logs) for before/after comparisons.
///
/// Only regular files under `objects/`, `refs/`, and `logs/` are included, sorted by relative path.
#[must_use]
pub fn git_dir_state_digest(git_dir: &Path) -> String {
    let mut paths = Vec::new();
    for sub in ["objects", "refs", "logs"] {
        let root = git_dir.join(sub);
        if root.is_dir() {
            collect_git_state_files(&root, &mut paths);
        }
    }
    paths.sort();
    let mut hasher = Sha1::new();
    for path in paths {
        let rel = path
            .strip_prefix(git_dir)
            .unwrap_or(&path)
            .to_string_lossy();
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = fs::read(&path) {
            hasher.update(bytes);
        }
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

fn collect_git_state_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            collect_git_state_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

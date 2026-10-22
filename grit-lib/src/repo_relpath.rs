//! Repository-relative paths as raw bytes (Git index paths).
//!
//! Index and tree paths are byte sequences (Git slash-separated). On Unix the work tree
//! uses the same bytes as file names. APIs that surface paths as `String` use a reversible
//! ASCII encoding for non-UTF-8 bytes so staging and diff can round-trip without loss.
//!
//! ```rust
//! use grit_lib::repo_relpath::file_name;
//! assert_eq!(file_name(b"src/lib.rs"), b"lib.rs");
//! ```

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::index::worktree_path_from_index_rel;
use crate::quote_path::{quote_c_style, quote_c_style_bytes};

const STORAGE_PREFIX: &str = "grit+pathhex:";

/// Parent directory prefix of a repository-relative path (empty when at repository root).
#[must_use]
pub fn parent_dir(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => Vec::new(),
        Some(pos) => path[..pos].to_vec(),
        None => Vec::new(),
    }
}

/// Final path component (file or directory name).
#[must_use]
pub fn file_name(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(pos) if pos + 1 < path.len() => &path[pos + 1..],
        _ => path,
    }
}

/// Join a stored prefix path with a tree or index name segment (raw bytes).
#[must_use]
pub fn format_path_stored(prefix: &str, name: &[u8]) -> String {
    let prefix_bytes = storage_bytes(prefix);
    storage_string(&join_rel(&prefix_bytes, name))
}

#[must_use]
pub fn join_rel(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(parent.len() + 1 + name.len());
    out.extend_from_slice(parent);
    out.push(b'/');
    out.extend_from_slice(name);
    out
}

/// Store index/worktree-relative bytes in a `String` (UTF-8 when valid, else hex encoding).
#[must_use]
pub fn storage_string(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(s) if !s.starts_with(STORAGE_PREFIX) => s.to_owned(),
        _ => {
            let mut out = String::with_capacity(STORAGE_PREFIX.len() + path.len() * 2);
            out.push_str(STORAGE_PREFIX);
            for b in path {
                use std::fmt::Write;
                let _ = write!(out, "{b:02x}");
            }
            out
        }
    }
}

/// Decode a path previously produced by [`storage_string`].
#[must_use]
pub fn storage_bytes(stored: &str) -> Cow<'_, [u8]> {
    if let Some(hex) = stored.strip_prefix(STORAGE_PREFIX) {
        if hex.len() % 2 != 0 {
            return Cow::Borrowed(stored.as_bytes());
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        let bytes = hex.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            let hi = hex_nibble(bytes[i]);
            let lo = hex_nibble(bytes[i + 1]);
            match (hi, lo) {
                (Some(h), Some(l)) => out.push((h << 4) | l),
                _ => return Cow::Borrowed(stored.as_bytes()),
            }
            i += 2;
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(stored.as_bytes())
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Absolute work tree path for a stored repository-relative path string.
#[must_use]
pub fn worktree_abs(work_tree: &Path, stored: &str) -> PathBuf {
    let rel = storage_bytes(stored);
    worktree_path_from_index_rel(work_tree, &rel)
}

/// Human-facing path (Git C-style quoting when needed).
#[must_use]
pub fn display(stored: &str, quote_fully: bool) -> String {
    let bytes = storage_bytes(stored);
    match std::str::from_utf8(&bytes) {
        Ok(s) => quote_c_style(s, quote_fully),
        Err(_) => quote_c_style_bytes(&bytes, true),
    }
}

/// UTF-8 path for APIs that require `&str` (ignore rules, attributes) when possible.
#[must_use]
pub fn as_utf8_for_matching(stored: &str) -> Cow<'_, str> {
    match stored.strip_prefix(STORAGE_PREFIX) {
        None => Cow::Borrowed(stored),
        Some(_) => Cow::Owned(String::from_utf8_lossy(&storage_bytes(stored)).into_owned()),
    }
}

/// Build a stored path string from work tree prefix stripping (Unix byte-preserving).
#[cfg(unix)]
pub fn strip_worktree_prefix_stored(path: &Path, work_tree: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let wt = work_tree.as_os_str().as_bytes();
    let p = path.as_os_str().as_bytes();
    if !p.starts_with(wt) {
        return None;
    }
    let rest = &p[wt.len()..];
    let rel = rest.strip_prefix(b"/").unwrap_or(rest);
    Some(storage_string(rel))
}

#[cfg(not(unix))]
pub fn strip_worktree_prefix_stored(path: &Path, work_tree: &Path) -> Option<String> {
    crate::git_path::strip_worktree_prefix(path, work_tree).map(|s| storage_string(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_round_trip_utf8() {
        let s = storage_string(b"src/a.rs");
        assert_eq!(s, "src/a.rs");
        assert_eq!(storage_bytes(&s)[..], b"src/a.rs"[..]);
    }

    #[test]
    fn storage_round_trip_non_utf8() {
        let raw = b"bad\xffname.txt";
        let s = storage_string(raw);
        assert!(s.starts_with(STORAGE_PREFIX));
        assert_eq!(storage_bytes(&s)[..], raw[..]);
    }

    #[test]
    fn join_and_parent() {
        assert_eq!(parent_dir(b"a/b/c"), b"a/b");
        assert_eq!(file_name(b"a/b/c"), b"c");
        assert_eq!(join_rel(b"a", b"b"), b"a/b");
    }

    #[test]
    fn storage_escapes_utf8_that_looks_like_hex_encoding() {
        let literal = b"grit+pathhex:6162";
        let stored = storage_string(literal);
        assert_ne!(stored, "grit+pathhex:6162");
        assert!(stored.starts_with(STORAGE_PREFIX));
        assert_eq!(storage_bytes(&stored)[..], literal[..]);
    }
}

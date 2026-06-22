//! Byte-correct repository paths.
//!
//! Git stores pathnames in trees, the index, and on the wire as raw byte
//! strings (only `NUL` is forbidden, and `/` separates components). On Unix the
//! OS agrees: `OsStr` is bytes. Representing these paths as `String`/`&str`
//! forces a lossy `from_utf8_lossy` that replaces invalid bytes with U+FFFD,
//! silently corrupting non-UTF-8 names.
//!
//! [`RepoPathBuf`] / [`RepoPath`] are an owned/borrowed pair (mirroring
//! `PathBuf`/`Path`) that hold the bytes losslessly. They are a *dedicated*
//! type, not a bare `BString`: a value of this type means "a repo-relative,
//! `/`-separated Git path". Conversions to/from `str` and the filesystem are
//! explicit, so a path can never implicitly masquerade as an arbitrary byte
//! blob (the `gix`/`BString` pitfall).

use bstr::ByteSlice;
use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// An owned, repo-relative, `/`-separated path in Git's byte encoding.
///
/// Invariants the type *intends* (not enforced on construction): no interior
/// `NUL`, `/`-separated. Normalization of `.`/`..` is the caller's job.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPathBuf(Vec<u8>);

/// Borrowed counterpart of [`RepoPathBuf`]; `&RepoPath` is to `RepoPathBuf` as
/// `&Path` is to `PathBuf`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RepoPath([u8]);

impl RepoPath {
    /// Borrow a byte slice as a repo path. Bytes are taken verbatim.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> &RepoPath {
        // SAFETY: `RepoPath` is `#[repr(transparent)]` over `[u8]`, so `&[u8]`
        // and `&RepoPath` have identical layout. This mirrors how `std::path::Path`
        // wraps `OsStr`.
        unsafe { &*(bytes as *const [u8] as *const RepoPath) }
    }

    /// Borrow a `&str` as a repo path (UTF-8 is a subset of valid byte paths).
    #[must_use]
    #[allow(clippy::should_implement_trait)] // intentional inherent ctor, mirrors `Path::new`
    pub fn from_str(s: &str) -> &RepoPath {
        RepoPath::from_bytes(s.as_bytes())
    }

    /// The raw bytes of the path.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// `true` if the path is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The path as `&str` iff it is valid UTF-8, else `None`. Use this for
    /// programmatic comparison; never for human display (see [`RepoPath::display`]).
    #[must_use]
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// A lossy `Display` adapter for humans/logs. Invalid bytes render as
    /// U+FFFD. User-facing command output that must round-trip should route
    /// through `quote_path` instead.
    #[must_use]
    pub fn display(&self) -> RepoPathDisplay<'_> {
        RepoPathDisplay(&self.0)
    }

    /// The final component (after the last `/`), or `None` if empty or ends in `/`.
    #[must_use]
    pub fn file_name(&self) -> Option<&RepoPath> {
        if self.0.is_empty() {
            return None;
        }
        match self.0.rfind_byte(b'/') {
            Some(i) => {
                let last = &self.0[i + 1..];
                if last.is_empty() {
                    None
                } else {
                    Some(RepoPath::from_bytes(last))
                }
            }
            None => Some(self),
        }
    }

    /// The path up to (but not including) the last `/`, or `None` if there is
    /// no separator.
    #[must_use]
    pub fn parent(&self) -> Option<&RepoPath> {
        self.0
            .rfind_byte(b'/')
            .map(|i| RepoPath::from_bytes(&self.0[..i]))
    }

    /// Iterate the `/`-separated components, skipping empty ones.
    pub fn components(&self) -> impl Iterator<Item = &RepoPath> {
        self.0
            .split(|&b| b == b'/')
            .filter(|c| !c.is_empty())
            .map(RepoPath::from_bytes)
    }

    /// Join another repo path onto this one with a `/` separator.
    #[must_use]
    pub fn join(&self, other: &RepoPath) -> RepoPathBuf {
        let mut out = Vec::with_capacity(self.0.len() + 1 + other.0.len());
        out.extend_from_slice(&self.0);
        if !self.0.is_empty() && !self.0.ends_with(b"/") {
            out.push(b'/');
        }
        out.extend_from_slice(&other.0);
        RepoPathBuf(out)
    }

    /// Resolve this repo-relative path against an on-disk `root`, producing a
    /// real filesystem path. This is the single OS boundary: on Unix the bytes
    /// map directly to an `OsStr`; on other platforms they go through a lossy
    /// UTF-8 conversion (those platforms cannot represent arbitrary bytes in
    /// filenames anyway).
    #[must_use]
    pub fn to_fs_path(&self, root: &Path) -> PathBuf {
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            root.join(OsStr::from_bytes(&self.0))
        }
        #[cfg(not(unix))]
        {
            // `Path` accepts `/` as a separator on Windows.
            root.join(&*String::from_utf8_lossy(&self.0))
        }
    }
}

impl RepoPathBuf {
    /// Take ownership of raw bytes as a repo path.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> RepoPathBuf {
        RepoPathBuf(bytes)
    }

    /// Build a repo path from a UTF-8 `String` (lossless; UTF-8 ⊂ byte paths).
    #[must_use]
    pub fn from_string(s: String) -> RepoPathBuf {
        RepoPathBuf(s.into_bytes())
    }

    /// Build a repo-relative path from a filesystem path. On Unix the `OsStr`
    /// bytes are taken verbatim; elsewhere `\` separators are normalized to `/`.
    #[must_use]
    pub fn from_fs_relative(path: &Path) -> RepoPathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            RepoPathBuf(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(not(unix))]
        {
            RepoPathBuf(path.to_string_lossy().replace('\\', "/").into_bytes())
        }
    }

    /// Consume into the underlying bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl Deref for RepoPathBuf {
    type Target = RepoPath;

    fn deref(&self) -> &RepoPath {
        RepoPath::from_bytes(&self.0)
    }
}

impl Borrow<RepoPath> for RepoPathBuf {
    fn borrow(&self) -> &RepoPath {
        self
    }
}

impl ToOwned for RepoPath {
    type Owned = RepoPathBuf;

    fn to_owned(&self) -> RepoPathBuf {
        RepoPathBuf(self.0.to_vec())
    }
}

impl AsRef<RepoPath> for RepoPath {
    fn as_ref(&self) -> &RepoPath {
        self
    }
}

impl AsRef<RepoPath> for RepoPathBuf {
    fn as_ref(&self) -> &RepoPath {
        self
    }
}

/// Lossy `Display`/`Debug` adapter returned by [`RepoPath::display`].
pub struct RepoPathDisplay<'a>(&'a [u8]);

impl fmt::Display for RepoPathDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `BStr`'s Display renders invalid UTF-8 as U+FFFD without allocating.
        fmt::Display::fmt(self.0.as_bstr(), f)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.0.as_bstr())
    }
}

impl fmt::Debug for RepoPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPathBuf({:?})", self.0.as_bstr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-UTF-8 byte path survives a full round-trip and is reported as
    /// non-UTF-8 by `to_str`.
    #[test]
    fn non_utf8_round_trip() {
        let raw = b"caf\xe9/\xff.txt";
        let p = RepoPathBuf::from_bytes(raw.to_vec());

        assert_eq!(p.as_bytes(), raw);
        assert!(p.to_str().is_none(), "invalid UTF-8 must not decode");
        // Lossy display inserts replacement chars but never panics.
        assert!(p.display().to_string().contains('\u{fffd}'));
    }

    #[test]
    fn components_file_name_parent() {
        let p = RepoPathBuf::from_string("a/b/c.txt".into());
        let comps: Vec<_> = p.components().map(|c| c.as_bytes().to_vec()).collect();
        assert_eq!(comps, vec![b"a".to_vec(), b"b".to_vec(), b"c.txt".to_vec()]);
        assert_eq!(p.file_name().map(RepoPath::as_bytes), Some(&b"c.txt"[..]));
        assert_eq!(p.parent().map(RepoPath::as_bytes), Some(&b"a/b"[..]));

        let top = RepoPathBuf::from_string("readme".into());
        assert_eq!(top.file_name().map(RepoPath::as_bytes), Some(&b"readme"[..]));
        assert!(top.parent().is_none());
    }

    #[test]
    fn join_inserts_single_separator() {
        let a = RepoPathBuf::from_string("a/b".into());
        let joined = a.join(RepoPath::from_str("c"));
        assert_eq!(joined.as_bytes(), b"a/b/c");

        // Empty base does not produce a leading separator.
        let empty = RepoPathBuf::default();
        assert_eq!(empty.join(RepoPath::from_str("x")).as_bytes(), b"x");
    }

    #[cfg(unix)]
    #[test]
    fn fs_path_round_trip_unix() {
        let raw = b"sub/\xe9dir/file.txt";
        let p = RepoPathBuf::from_bytes(raw.to_vec());
        let fs = p.to_fs_path(Path::new("/tmp/repo"));
        // Round-trips back to the same repo-relative bytes when re-derived.
        let back = RepoPathBuf::from_fs_relative(Path::new(&fs));
        assert!(back.as_bytes().ends_with(raw));
    }
}

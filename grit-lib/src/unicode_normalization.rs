//! UTF-8 NFC path normalization for macOS-style filesystems (`core.precomposeUnicode`).
//!
//! When the filesystem treats NFD and NFC spellings as the same path, Git stores paths in
//! precomposed (NFC) form. This module implements the same normalization using ICU.

use icu_normalizer::ComposingNormalizerBorrowed;
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Return true if `s` contains any non-ASCII UTF-8 byte.
#[must_use]
pub fn has_non_ascii_utf8(s: &str) -> bool {
    s.as_bytes().iter().any(|b| *b & 0x80 != 0)
}

/// Normalize a single path segment (no `/`) to NFC when it contains non-ASCII UTF-8.
#[must_use]
pub fn precompose_utf8_segment(s: &str) -> Cow<'_, str> {
    if !has_non_ascii_utf8(s) {
        return Cow::Borrowed(s);
    }
    let normalized = ComposingNormalizerBorrowed::new_nfc().normalize(s);
    if normalized == s {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(normalized.into_owned())
    }
}

/// Normalize every `/`-separated segment of `path` to NFC.
#[must_use]
pub fn precompose_utf8_path(path: &str) -> Cow<'_, str> {
    if !path.as_bytes().iter().any(|b| *b & 0x80 != 0) {
        return Cow::Borrowed(path);
    }
    let mut buf = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            buf.push('/');
        }
        let c = precompose_utf8_segment(seg);
        buf.push_str(c.as_ref());
    }
    if buf == path {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(buf)
    }
}

/// Update `s` in place when it is valid UTF-8 and NFC differs from the current spelling.
pub fn precompose_os_string_utf8_path(s: &mut OsString, enabled: bool) {
    if !enabled {
        return;
    }
    let Some(utf8) = s.to_str() else {
        return;
    };
    let normalized = precompose_utf8_path(utf8).into_owned();
    if normalized != utf8 {
        *s = OsString::from(normalized);
    }
}

/// Probe whether creating a file under `git_dir` with an NFC filename makes the NFD spelling
/// visible as the same path (macOS / HFS+ style).
///
/// Matches Git's `probe_utf8_pathname_composition` / `UTF8_NFD_TO_NFC` test prerequisite.
/// Uses `create_new` like Git's `O_EXCL`: if the probe filename already exists (e.g. copied from
/// an init template), the probe is skipped and the existing file is left untouched.
pub fn probe_filesystem_normalizes_nfd_to_nfc(git_dir: &Path) -> std::io::Result<bool> {
    const NFC: &str = "\u{00e4}";
    const NFD: &str = "\u{0061}\u{0308}";
    let nfc_path: PathBuf = git_dir.join(NFC);
    let mut f = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&nfc_path)
    {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    f.write_all(b"x")?;
    let nfd_path = git_dir.join(NFD);
    let aliases = nfd_path.exists();
    let _ = fs::remove_file(&nfc_path);
    Ok(aliases)
}

/// Absolute worktree path and repository-relative index spelling after NFC/NFD resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePathForStaging {
    /// Path to open on disk (actual directory entry spelling).
    pub abs: PathBuf,
    /// Repository-relative path stored in the index and trees (NFC when precompose is enabled).
    pub index_relpath: String,
}

/// Resolve a repository-relative path to the on-disk file and the index spelling Git uses.
///
/// When `precompose_unicode` is true, walks path components and matches directory entries by
/// NFC-normalized segment names so an NFD worktree name (common on macOS, or Linux with
/// `GIT_TEST_UTF8_NFD_TO_NFC`) still opens the correct file while the index stores NFC bytes.
///
/// # Parameters
///
/// - `work_tree` — repository root.
/// - `rel` — path relative to the work tree (may be NFD or NFC).
/// - `precompose_unicode` — whether `core.precomposeunicode` is effectively enabled.
#[must_use]
pub fn resolve_worktree_path_for_staging(
    work_tree: &Path,
    rel: &str,
    precompose_unicode: bool,
) -> WorktreePathForStaging {
    let index_rel = |path: &str| -> String {
        if precompose_unicode {
            precompose_utf8_path(path).into_owned()
        } else {
            path.to_owned()
        }
    };

    let abs = work_tree.join(rel);
    if fs::symlink_metadata(&abs).is_ok() {
        return WorktreePathForStaging {
            abs,
            index_relpath: index_rel(rel),
        };
    }
    if !precompose_unicode {
        return WorktreePathForStaging {
            abs,
            index_relpath: rel.to_owned(),
        };
    }

    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        return WorktreePathForStaging {
            abs,
            index_relpath: index_rel(rel),
        };
    }

    let mut disk_abs = work_tree.to_path_buf();
    let mut disk_rel = String::new();
    for (i, component) in components.iter().enumerate() {
        let want = precompose_utf8_segment(component);
        let direct = disk_abs.join(component);
        if fs::symlink_metadata(&direct).is_ok() {
            disk_abs = direct;
            if !disk_rel.is_empty() {
                disk_rel.push('/');
            }
            disk_rel.push_str(component);
            continue;
        }
        let mut found: Option<String> = None;
        if let Ok(rd) = fs::read_dir(&disk_abs) {
            for ent in rd.flatten() {
                let n = ent.file_name().to_string_lossy().into_owned();
                if precompose_utf8_segment(&n).as_ref() == want.as_ref() {
                    found = Some(n);
                    break;
                }
            }
        }
        let Some(name) = found else {
            return WorktreePathForStaging {
                abs: work_tree.join(rel),
                index_relpath: index_rel(rel),
            };
        };
        disk_abs = disk_abs.join(&name);
        if !disk_rel.is_empty() {
            disk_rel.push('/');
        }
        disk_rel.push_str(&name);
        if i + 1 == components.len() {
            return WorktreePathForStaging {
                abs: disk_abs,
                index_relpath: index_rel(&disk_rel),
            };
        }
    }
    WorktreePathForStaging {
        abs: disk_abs,
        index_relpath: index_rel(&disk_rel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precompose_nfd_filename_to_nfc() {
        // Matches t3910: Adiarnfc = UTF-8 \303\204 (U+00C4), Adiarnfd = A + U+0308.
        let nfd = format!("f.{}\u{0308}", 'A');
        let nfc = format!("f.\u{00c4}");
        assert_eq!(precompose_utf8_path(&nfd).as_ref(), nfc.as_str());
    }

    #[test]
    fn resolve_nfc_index_path_for_nfd_on_disk() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let wt = td.path();
        let nfd = format!("cafe\u{0301}.txt");
        fs::write(wt.join(&nfd), b"x").expect("write");
        let nfc = "caf\u{00e9}.txt";
        let r = resolve_worktree_path_for_staging(wt, nfc, true);
        assert!(r.abs.is_file());
        assert_eq!(r.index_relpath.as_str(), nfc);
    }

    #[test]
    fn probe_does_not_remove_existing_template_file() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let git_dir = td.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir");
        const NFC: &str = "\u{00e4}";
        fs::write(git_dir.join(NFC), b"from-template").expect("template file");
        assert!(!probe_filesystem_normalizes_nfd_to_nfc(&git_dir).expect("probe"));
        assert_eq!(fs::read(git_dir.join(NFC)).expect("read"), b"from-template");
    }
}

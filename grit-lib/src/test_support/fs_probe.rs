//! Runtime probes for filesystem behavior that tests depend on.
//!
//! Some tests need a capability the host volume may lack: macOS APFS folds ASCII
//! case, treats NFC and NFD spellings as one name, and refuses file names that are
//! not valid UTF-8. Each probe creates a throwaway entry inside the given directory
//! (normally the test's temp dir, so it measures the same volume the test uses),
//! inspects it, and removes it again. Probe failures report the capability as absent
//! (or, for the folding probes, as not folding) rather than panicking.

use std::fs;
use std::path::{Path, PathBuf};

/// Create an empty probe file named `name` under `dir`, call `check`, then remove it.
///
/// Returns `None` when the probe file cannot be created.
fn with_probe_file(dir: &Path, name: &str, check: impl FnOnce(&Path) -> bool) -> Option<bool> {
    let path = dir.join(name);
    fs::write(&path, b"").ok()?;
    let result = check(&path);
    let _ = fs::remove_file(&path);
    Some(result)
}

/// An unused probe name in `dir` built from `stem` and a counter.
fn unused_name(dir: &Path, stem: &str) -> Option<String> {
    (0..1024_u32)
        .map(|n| format!(".grit-fs-probe-{stem}-{n}"))
        .find(|name| !dir.join(name).exists())
}

/// Whether `dir`'s filesystem resolves names that differ only in ASCII case to one entry.
///
/// # Parameters
///
/// - `dir`: an existing, writable directory on the volume under test.
///
/// # Returns
///
/// `true` on case-insensitive volumes (default APFS, NTFS); `false` on case-sensitive
/// ones or when the probe cannot be written.
#[must_use]
pub fn is_case_insensitive(dir: &Path) -> bool {
    let Some(name) = unused_name(dir, "icase") else {
        return false;
    };
    with_probe_file(dir, &name, |_| dir.join(name.to_ascii_uppercase()).exists()).unwrap_or(false)
}

/// Whether `dir`'s filesystem resolves the NFD spelling of a name to its NFC entry.
///
/// # Parameters
///
/// - `dir`: an existing, writable directory on the volume under test.
///
/// # Returns
///
/// `true` on normalization-insensitive volumes (APFS, HFS+); `false` otherwise or when
/// the probe cannot be written.
#[must_use]
pub fn is_normalization_insensitive(dir: &Path) -> bool {
    let Some(stem) = unused_name(dir, "nfc") else {
        return false;
    };
    let nfc = format!("{stem}-\u{e9}");
    let nfd = format!("{stem}-e\u{301}");
    with_probe_file(dir, &nfc, |_| dir.join(&nfd).exists()).unwrap_or(false)
}

/// Whether `dir`'s filesystem accepts file names that are not valid UTF-8.
///
/// # Parameters
///
/// - `dir`: an existing, writable directory on the volume under test.
///
/// # Returns
///
/// `true` when a name containing a stray `0xff` byte can be created (typical Linux
/// filesystems); `false` when creation fails, as on APFS (`EILSEQ`).
#[cfg(unix)]
#[must_use]
pub fn accepts_non_utf8_names(dir: &Path) -> bool {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let Some(stem) = unused_name(dir, "raw") else {
        return false;
    };
    let mut raw = stem.into_bytes();
    raw.push(0xff);
    let path: PathBuf = dir.join(OsStr::from_bytes(&raw));
    if fs::write(&path, b"").is_err() {
        return false;
    }
    let _ = fs::remove_file(&path);
    true
}

/// Whether `dir`'s filesystem accepts file names that are not valid UTF-8.
///
/// Non-Unix hosts cannot spell such names through [`std::path::Path`], so this is
/// always `false` there.
#[cfg(not(unix))]
#[must_use]
pub fn accepts_non_utf8_names(_dir: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_leave_the_directory_empty() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _ = is_case_insensitive(tmp.path());
        let _ = is_normalization_insensitive(tmp.path());
        let _ = accepts_non_utf8_names(tmp.path());
        let left: Vec<_> = fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(left.is_empty(), "probe entries left behind: {left:?}");
    }

    #[test]
    fn missing_directory_reports_capabilities_absent() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let gone = tmp.path().join("missing");
        assert!(!is_case_insensitive(&gone));
        assert!(!is_normalization_insensitive(&gone));
        assert!(!accepts_non_utf8_names(&gone));
    }
}

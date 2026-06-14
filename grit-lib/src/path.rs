//! Git-compatible verification of tree/index path components.
//!
//! Ports Git's `verify_path()` / `verify_dotfile()` (`read-cache.c`) plus the
//! HFS+ and NTFS `.git` equivalence checks from `path.c`. These guard against
//! malicious trees whose entries name `.git` (or a filesystem-folding alias of
//! it) and would otherwise let a `checkout`/`clone` write into the repository's
//! own `.git` directory (CVE-2014-9390).
//!
//! The same primitives are used both when reading a tree into the index
//! (`read-tree`) and when writing index entries out to the working tree
//! (`checkout`), so that the byte-writing path is protected even if a crafted
//! tree bypassed `read-tree`.

use std::path::Path;

use crate::config::ConfigSet;
use crate::error::Error;

/// Path protection settings from `core.protectHFS` / `core.protectNTFS`.
#[derive(Clone, Copy, Debug)]
pub struct PathProtection {
    /// Reject paths that HFS+ would fold onto `.git` (`core.protectHFS`).
    pub protect_hfs: bool,
    /// Reject paths that NTFS would fold onto `.git` (`core.protectNTFS`).
    pub protect_ntfs: bool,
}

impl PathProtection {
    /// Load protection settings from the repository config at `git_dir`,
    /// applying Git's platform defaults when unset.
    #[must_use]
    pub fn load(git_dir: &Path) -> Self {
        let config = ConfigSet::load(Some(git_dir), true).unwrap_or_else(|_| ConfigSet::new());
        Self::from_config(&config)
    }

    /// Resolve protection settings from an already-loaded [`ConfigSet`].
    ///
    /// Mirrors Git's defaults (`environment.c`): `core.protectHFS` defaults on
    /// for Apple platforms, `core.protectNTFS` defaults on everywhere
    /// (`PROTECT_NTFS_DEFAULT`). An explicit config value overrides the default.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let protect_hfs = config
            .get("core.protectHFS")
            .map_or(cfg!(target_os = "macos"), |v| v.eq_ignore_ascii_case("true"));
        let protect_ntfs = config
            .get("core.protectNTFS")
            .is_none_or(|v| v.eq_ignore_ascii_case("true"));
        Self {
            protect_hfs,
            protect_ntfs,
        }
    }
}

/// Verify every `/`-separated component of `path` is safe to materialize.
///
/// `path` is a repository-relative slash-separated tree/index path. Each
/// component is checked with [`verify_path_component`]. `is_symlink` is the
/// mode of the leaf entry being written and gates the `.gitmodules` checks,
/// matching Git's `verify_path()`.
///
/// # Errors
///
/// Returns [`Error::InvalidPath`] for the first forbidden component.
pub fn verify_path(path: &[u8], prot: PathProtection, is_symlink: bool) -> Result<(), Error> {
    for component in path.split(|b| *b == b'/') {
        verify_path_component(component, prot, is_symlink)?;
    }
    Ok(())
}

/// Check whether a single path component (file or directory name) is forbidden.
///
/// Mirrors Git's `verify_path()` / `verify_dotfile()`: `.`, `..`, and `.git`
/// (plus its HFS/NTFS folds) are always rejected; an HFS/NTFS-folded
/// `.gitmodules` is rejected only when the entry is a symlink (CVE-2018-11235).
/// Regular files named `.gitignore`, `.gitmodules`, `.mailmap`, etc. are
/// allowed — Git does not reject them in `verify_path`.
///
/// # Errors
///
/// Returns [`Error::InvalidPath`] when the name is rejected.
pub fn verify_path_component(
    name: &[u8],
    prot: PathProtection,
    is_symlink: bool,
) -> Result<(), Error> {
    let reject = || Error::InvalidPath(String::from_utf8_lossy(name).into_owned());

    // Always reject "." and ".."
    if name == b"." || name == b".." {
        return Err(reject());
    }

    // Always reject ".git" (exact lowercase — matches C git's verify_dotfile)
    if name == b".git" {
        return Err(reject());
    }

    // HFS / NTFS case-insensitive ".git" checks.
    if (prot.protect_hfs || prot.protect_ntfs)
        && name.len() == 4
        && name[0] == b'.'
        && name[1..].eq_ignore_ascii_case(b"git")
    {
        return Err(reject());
    }
    if prot.protect_hfs {
        if hfs_equivalent_to_dotgit(name) {
            return Err(reject());
        }
        // A symlink whose name folds to `.gitmodules` on HFS+ is the
        // CVE-2018-11235 vector; reject it (Git: `is_hfs_dotgitmodules`).
        if is_symlink {
            if let Ok(s) = std::str::from_utf8(name) {
                if crate::dotfile::is_hfs_dot_gitmodules(s) {
                    return Err(reject());
                }
            }
        }
    }

    if prot.protect_ntfs {
        // NTFS short-name check: "git~1" (case-insensitive)
        if name.eq_ignore_ascii_case(b"git~1") {
            return Err(reject());
        }
        // Backslashes are treated as path separators on NTFS, so reject
        // confusing names that rely on '\' being a regular byte.
        if name.contains(&b'\\') {
            return Err(reject());
        }
        // Reject NTFS-equivalent ".git" names such as ".git ", ".git...",
        // and alternate stream forms like ".git...:stream".
        if ntfs_equivalent_to_dotgit(name) {
            return Err(reject());
        }
        // Symlink folding to `.gitmodules` on NTFS (CVE-2018-11235).
        if is_symlink {
            if let Ok(s) = std::str::from_utf8(name) {
                if crate::dotfile::is_ntfs_dot_gitmodules(s) {
                    return Err(reject());
                }
            }
        }
    }

    Ok(())
}

fn ntfs_equivalent_to_dotgit(name: &[u8]) -> bool {
    if name.len() < 4 || !name[..4].eq_ignore_ascii_case(b".git") {
        return false;
    }

    let rest = &name[4..];
    if rest.is_empty() {
        return true;
    }

    let head = rest.split(|b| *b == b':').next().unwrap_or(rest);
    let mut trimmed_len = head.len();
    while trimmed_len > 0 && matches!(head[trimmed_len - 1], b'.' | b' ') {
        trimmed_len -= 1;
    }

    trimmed_len == 0
}

fn hfs_equivalent_to_dotgit(name: &[u8]) -> bool {
    let Ok(path) = std::str::from_utf8(name) else {
        return false;
    };

    let folded: String = path
        .chars()
        .filter(|ch| !matches!(*ch, '\u{200c}' | '\u{200d}'))
        .flat_map(char::to_lowercase)
        .collect();
    folded == ".git"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> PathProtection {
        PathProtection {
            protect_hfs: true,
            protect_ntfs: true,
        }
    }

    fn off() -> PathProtection {
        PathProtection {
            protect_hfs: false,
            protect_ntfs: false,
        }
    }

    #[test]
    fn rejects_dotgit_always() {
        assert!(verify_path_component(b".git", off(), false).is_err());
        assert!(verify_path_component(b".", off(), false).is_err());
        assert!(verify_path_component(b"..", off(), false).is_err());
    }

    #[test]
    fn rejects_case_and_alias_under_protection() {
        for name in [
            &b".Git"[..],
            b".GIT",
            b"git~1",
            b".git ",
            b".git...",
            b".git\\foo",
        ] {
            assert!(
                verify_path_component(name, both(), false).is_err(),
                "expected rejection of {:?}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn rejects_hfs_ignorable_dotgit() {
        // ".gi‌t" folds to ".git" on HFS+.
        let name = ".gi\u{200c}t".as_bytes();
        assert!(verify_path_component(name, both(), false).is_err());
        // Allowed when HFS protection is off (only NTFS on).
        let ntfs_only = PathProtection {
            protect_hfs: false,
            protect_ntfs: true,
        };
        assert!(verify_path_component(name, ntfs_only, false).is_ok());
    }

    #[test]
    fn rejects_dotgit_directory_component() {
        // A crafted ".Git/config" path: the directory component is rejected.
        assert!(verify_path(b".Git/config", both(), false).is_err());
        // The HFS ignorable-codepoint alias as a directory component is rejected too.
        assert!(verify_path(".gi\u{200c}t/config".as_bytes(), both(), false).is_err());
    }

    #[test]
    fn allows_normal_paths() {
        // Regular dotfiles are allowed even with both protections on — Git does
        // not reject these in verify_path (only symlinked `.gitmodules` folds).
        assert!(verify_path(b"src/main.rs", both(), false).is_ok());
        assert!(verify_path(b".gitconfig", both(), false).is_ok());
        assert!(verify_path(b".gitignore", both(), false).is_ok());
        assert!(verify_path(b".gitmodules", both(), false).is_ok());
        assert!(verify_path(b".gitattributes", both(), false).is_ok());
        assert!(verify_path(b".mailmap", both(), false).is_ok());
        assert!(verify_path(b".github/workflows/ci.yml", both(), false).is_ok());
        assert!(verify_path_component(b"gitconfig", both(), false).is_ok());
    }

    #[test]
    fn rejects_symlinked_gitmodules_fold() {
        // ".gitmodules" folded via an HFS ignorable codepoint, as a symlink, is
        // the CVE-2018-11235 vector and must be rejected — but only as a symlink.
        let name = ".gitmodule\u{200c}s".as_bytes();
        assert!(verify_path_component(name, both(), true).is_err());
        assert!(verify_path_component(name, both(), false).is_ok());
        // An exact ".gitmodules" symlink is the classic vector and is also rejected,
        // while a regular file of the same name is allowed.
        assert!(verify_path_component(b".gitmodules", both(), true).is_err());
        assert!(verify_path_component(b".gitmodules", both(), false).is_ok());
    }
}

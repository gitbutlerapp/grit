//! Git-compatible path component validation (`verify_path` / tree fsck rules).
//!
//! Centralizes NTFS/HFS dot-file checks and tree-entry rules that previously
//! lived in separate internal modules. Embedders use this before writing index entries,
//! checking out paths, or building trees; [`ProtectOptions`] mirrors
//! `core.protectNTFS` and `core.protectHFS`.
//!
//! ```rust
//! use grit_lib::path_validate::{
//!     validate_path, EntryMode, PathComponentError, PathValidationKind, ProtectOptions,
//! };
//!
//! let opts = ProtectOptions::default();
//! let blob = EntryMode::new(0o100644);
//! let kind = PathValidationKind::Worktree;
//! validate_path(b"src/lib.rs", blob, opts, kind).unwrap();
//! assert_eq!(
//!     validate_path(b"GIT~1./hooks", blob, opts, kind),
//!     Err(PathComponentError::NtfsDotGit)
//! );
//! ```

use crate::config::ConfigSet;
use crate::dotfile::{
    is_hfs_dot_gitattributes, is_hfs_dot_gitignore, is_hfs_dot_gitmodules, is_hfs_dot_mailmap,
    is_hfs_dotgit, is_ntfs_dot_gitattributes, is_ntfs_dot_gitignore, is_ntfs_dot_gitmodules,
    is_ntfs_dot_mailmap, is_ntfs_dotgit,
};
use thiserror::Error;

/// Git tree/index file mode for validation (symlink vs blob affects dot-file rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryMode(u32);

impl EntryMode {
    /// Wrap a raw Git mode octal value (for example `0o100644`).
    #[must_use]
    pub const fn new(mode: u32) -> Self {
        Self(mode)
    }

    /// `true` when this entry is a symbolic link (`0o120000`).
    #[must_use]
    pub fn is_symlink(self) -> bool {
        self.0 == 0o120000
    }
}

/// Which Git path rules to apply ([`validate_component`] / [`validate_path`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PathValidationKind {
    /// Object fsck: all symlink dot-file rules; NTFS spellings when [`ProtectOptions`] say so.
    Fsck,
    /// Index population, checkout, and staging (Git `verify_path` on this platform).
    #[default]
    Worktree,
}

/// Whether NTFS and HFS+ spelling protections apply (from `core.protectNTFS` / `core.protectHFS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtectOptions {
    /// Reject NTFS `.git` spellings, short names, `\\` segments, and `:` streams.
    ///
    /// On Windows, worktree validation additionally rejects reserved device names,
    /// trailing spaces or periods, and characters Win32 does not allow in file names.
    pub protect_ntfs: bool,
    /// Reject HFS+ Unicode-ignorable `.git` spellings.
    pub protect_hfs: bool,
}

impl Default for ProtectOptions {
    fn default() -> Self {
        Self {
            protect_ntfs: true,
            protect_hfs: cfg!(target_os = "macos"),
        }
    }
}

impl ProtectOptions {
    /// Load protection flags from a merged [`ConfigSet`] (no environment reads).
    ///
    /// Defaults match Git: `core.protectNTFS` true; `core.protectHFS` true on macOS, false elsewhere.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let protect_ntfs = config
            .get_bool("core.protectNTFS")
            .or_else(|| config.get_bool("core.protectntfs"))
            .and_then(|r| r.ok())
            .unwrap_or(true);
        let default_hfs = cfg!(target_os = "macos");
        let protect_hfs = config
            .get_bool("core.protectHFS")
            .or_else(|| config.get_bool("core.protecthfs"))
            .and_then(|r| r.ok())
            .unwrap_or(default_hfs);
        Self {
            protect_ntfs,
            protect_hfs,
        }
    }

    /// Protection settings used by object fsck (always check both spellings).
    #[must_use]
    pub const fn fsck() -> Self {
        Self {
            protect_ntfs: true,
            protect_hfs: true,
        }
    }
}

/// Why a single path component fails validation.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathComponentError {
    /// Zero-length component.
    #[error("path component is empty")]
    Empty,
    /// Component is exactly `.`.
    #[error("path component is a single dot")]
    Dot,
    /// Component is exactly `..`.
    #[error("path component is parent directory")]
    DotDot,
    /// Component names `.git` (ASCII case rules) or `git~1`.
    #[error("path component is a .git alias")]
    DotGit,
    /// A `/` appears inside one component (tree entries must be single segments).
    #[error("path component contains a slash")]
    ContainsSlash,
    /// A NUL byte appears in the component.
    #[error("path component contains a NUL byte")]
    ContainsNul,
    /// HFS+ ignorable code points spell `.git` or a protected dot-file.
    #[error("path component is an HFS .git alias")]
    HfsDotGit,
    /// NTFS trailing space/dot, short name, or stream spelling for `.git`.
    #[error("path component is an NTFS .git alias")]
    NtfsDotGit,
    /// Backslash introduces an NTFS `.git` segment in the component.
    #[error("path component contains a backslash before a .git alias")]
    NtfsBackslash,
    /// DOS drive prefix or NTFS alternate-data-stream syntax in the component.
    ///
    /// On Windows, worktree validation reports any `:` in a component this way.
    #[error("path component uses a forbidden NTFS drive or stream prefix")]
    NtfsDriveOrStream,
    /// Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
    /// `LPT0`–`LPT9`, `CONIN$`, `CONOUT$`), with or without an extension or trailing spaces.
    ///
    /// Only reported by [`PathValidationKind::Worktree`] on Windows.
    #[error("path component is a reserved Windows device name")]
    NtfsReservedName,
    /// Component ends in a space or period, which Windows silently strips.
    ///
    /// Only reported by [`PathValidationKind::Worktree`] on Windows.
    #[error("path component ends with a space or period")]
    NtfsTrailingDotOrSpace,
    /// Component contains a control character or one of `<>"|?*`.
    ///
    /// Only reported by [`PathValidationKind::Worktree`] on Windows.
    #[error("path component contains a character Windows does not allow")]
    NtfsInvalidChar,
    /// Symlink whose name matches `.gitmodules` (HFS or NTFS spelling).
    #[error("symbolic link must not be named .gitmodules")]
    SymlinkedDotGitmodules,
    /// Symlink whose name matches `.gitattributes` (HFS or NTFS spelling).
    #[error("symbolic link must not be named .gitattributes")]
    SymlinkedDotGitattributes,
    /// Symlink whose name matches `.gitignore` (HFS or NTFS spelling).
    #[error("symbolic link must not be named .gitignore")]
    SymlinkedDotGitignore,
    /// Symlink whose name matches `.mailmap` (HFS or NTFS spelling).
    #[error("symbolic link must not be named .mailmap")]
    SymlinkedDotMailmap,
}

/// Semantic problems detected on a tree entry name (fsck aggregates several at once).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntryNameAudit {
    /// Empty component.
    pub empty: bool,
    /// Component is `.`.
    pub dot: bool,
    /// Component is `..`.
    pub dotdot: bool,
    /// Slash inside the component.
    pub slash: bool,
    /// NUL inside the component.
    pub nul: bool,
    /// Any `.git` alias (ASCII, HFS, NTFS, backslash, or `git~1`).
    pub dotgit: bool,
}

impl TreeEntryNameAudit {
    /// Merge another audit into this one (bitwise OR of flags).
    pub fn absorb(&mut self, other: Self) {
        self.empty |= other.empty;
        self.dot |= other.dot;
        self.dotdot |= other.dotdot;
        self.slash |= other.slash;
        self.nul |= other.nul;
        self.dotgit |= other.dotgit;
    }
}

/// Validate one path component (no `/` splitting).
///
/// # Errors
///
/// Returns the first [`PathComponentError`] encountered in Git fsck priority order
/// when multiple rules apply.
pub fn validate_component(
    name: &[u8],
    mode: EntryMode,
    opts: ProtectOptions,
    kind: PathValidationKind,
) -> Result<(), PathComponentError> {
    if name.is_empty() {
        return Err(PathComponentError::Empty);
    }
    if name.contains(&0) {
        return Err(PathComponentError::ContainsNul);
    }
    if name.contains(&b'/') {
        return Err(PathComponentError::ContainsSlash);
    }
    if let Some(err) = validate_component_symlink_dotfile(name, mode, opts, kind) {
        return Err(err);
    }
    if opts.protect_ntfs && ntfs_path_syntax_rules_apply() {
        if has_dos_drive_prefix_bytes(name) {
            return Err(PathComponentError::NtfsDriveOrStream);
        }
        if let Ok(name_str) = std::str::from_utf8(name) {
            if scan_ntfs_backslash_dot_special(name_str, ntfs_backslash_dotdot_tail) {
                return Err(PathComponentError::NtfsBackslash);
            }
        }
    }
    let audit = audit_tree_entry_name(name, opts);
    if audit.empty {
        return Err(PathComponentError::Empty);
    }
    if audit.slash {
        return Err(PathComponentError::ContainsSlash);
    }
    if audit.nul {
        return Err(PathComponentError::ContainsNul);
    }
    if audit.dot {
        return Err(PathComponentError::Dot);
    }
    if audit.dotdot {
        return Err(PathComponentError::DotDot);
    }
    if audit.dotgit {
        return Err(classify_dotgit_component_error(name, opts));
    }
    if opts.protect_ntfs && ntfs_path_syntax_rules_apply() && kind == PathValidationKind::Worktree {
        if let Some(err) = win32_component_error(name) {
            return Err(err);
        }
    }
    Ok(())
}

/// Validate a repository-relative path by checking each `/`-separated component.
///
/// # Errors
///
/// Same as [`validate_component`] for the first failing segment.
pub fn validate_path(
    path: &[u8],
    mode: EntryMode,
    opts: ProtectOptions,
    kind: PathValidationKind,
) -> Result<(), PathComponentError> {
    if path.is_empty() {
        return Err(PathComponentError::Empty);
    }
    if path.contains(&0) {
        return Err(PathComponentError::ContainsNul);
    }
    for component in path.split(|&b| b == b'/') {
        if component.is_empty() {
            return Err(PathComponentError::Empty);
        }
        validate_component(component, mode, opts, kind)?;
    }
    Ok(())
}

fn ntfs_path_syntax_rules_apply() -> bool {
    cfg!(windows)
}

/// Populate [`TreeEntryNameAudit`] for a tree object entry name (fsck uses all flags).
#[must_use]
pub fn audit_tree_entry_name(name: &[u8], opts: ProtectOptions) -> TreeEntryNameAudit {
    let mut audit = TreeEntryNameAudit::default();
    if name.is_empty() {
        audit.empty = true;
        return audit;
    }
    if name.contains(&0) {
        audit.nul = true;
    }
    if name == b"." {
        audit.dot = true;
    }
    if name == b".." {
        audit.dotdot = true;
    }
    if name.contains(&b'/') {
        audit.slash = true;
    }
    if component_is_dotgit_alias(name, opts) {
        audit.dotgit = true;
    }
    audit
}

impl TreeEntryNameAudit {
    /// Map aggregated flags to a single error (Git fsck reporting order).
    pub fn first_error(self) -> Result<(), PathComponentError> {
        if self.empty {
            return Err(PathComponentError::Empty);
        }
        if self.slash {
            return Err(PathComponentError::ContainsSlash);
        }
        if self.nul {
            return Err(PathComponentError::ContainsNul);
        }
        if self.dot {
            return Err(PathComponentError::Dot);
        }
        if self.dotdot {
            return Err(PathComponentError::DotDot);
        }
        if self.dotgit {
            return Err(PathComponentError::DotGit);
        }
        Ok(())
    }
}

/// Returns `true` when `name` should trigger fsck `hasDotgit` for a tree entry.
#[must_use]
pub fn component_is_dotgit_alias(name: &[u8], opts: ProtectOptions) -> bool {
    if name.eq_ignore_ascii_case(b".git") {
        return true;
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return false;
    };
    if opts.protect_hfs && is_hfs_dotgit(name_str) {
        return true;
    }
    if opts.protect_ntfs && (is_ntfs_dotgit(name_str) || scan_ntfs_backslash_dotgit(name_str)) {
        return true;
    }
    false
}

fn validate_component_symlink_dotfile(
    name: &[u8],
    mode: EntryMode,
    opts: ProtectOptions,
    kind: PathValidationKind,
) -> Option<PathComponentError> {
    if !mode.is_symlink() {
        return None;
    }
    if name.eq_ignore_ascii_case(b".gitmodules") {
        return Some(PathComponentError::SymlinkedDotGitmodules);
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return None;
    };
    if dot_gitmodules_spelling(name_str, opts) {
        return Some(PathComponentError::SymlinkedDotGitmodules);
    }
    if kind == PathValidationKind::Worktree {
        return None;
    }
    if name.eq_ignore_ascii_case(b".gitattributes") {
        return Some(PathComponentError::SymlinkedDotGitattributes);
    }
    if name.eq_ignore_ascii_case(b".gitignore") {
        return Some(PathComponentError::SymlinkedDotGitignore);
    }
    if name.eq_ignore_ascii_case(b".mailmap") {
        return Some(PathComponentError::SymlinkedDotMailmap);
    }
    if dot_gitattributes_spelling(name_str, opts) {
        return Some(PathComponentError::SymlinkedDotGitattributes);
    }
    if opts.protect_hfs && is_hfs_dot_gitignore(name_str) {
        return Some(PathComponentError::SymlinkedDotGitignore);
    }
    if opts.protect_hfs && is_hfs_dot_mailmap(name_str) {
        return Some(PathComponentError::SymlinkedDotMailmap);
    }
    if opts.protect_ntfs && is_ntfs_dot_gitignore(name_str) {
        return Some(PathComponentError::SymlinkedDotGitignore);
    }
    if opts.protect_ntfs && is_ntfs_dot_mailmap(name_str) {
        return Some(PathComponentError::SymlinkedDotMailmap);
    }
    None
}

fn dot_gitmodules_spelling(name: &str, opts: ProtectOptions) -> bool {
    (opts.protect_hfs && is_hfs_dot_gitmodules(name))
        || (opts.protect_ntfs && is_ntfs_dot_gitmodules(name))
        || (opts.protect_ntfs && scan_ntfs_backslash_dot_special(name, is_ntfs_dot_gitmodules))
}

fn dot_gitattributes_spelling(name: &str, opts: ProtectOptions) -> bool {
    (opts.protect_hfs && is_hfs_dot_gitattributes(name))
        || (opts.protect_ntfs && is_ntfs_dot_gitattributes(name))
        || (opts.protect_ntfs && scan_ntfs_backslash_dot_special(name, is_ntfs_dot_gitattributes))
}

fn classify_dotgit_component_error(name: &[u8], opts: ProtectOptions) -> PathComponentError {
    if name.eq_ignore_ascii_case(b".git") || name.eq_ignore_ascii_case(b"git~1") {
        return PathComponentError::DotGit;
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return PathComponentError::DotGit;
    };
    if opts.protect_ntfs && scan_ntfs_backslash_dotgit(name_str) {
        return PathComponentError::NtfsBackslash;
    }
    if opts.protect_hfs && is_hfs_dotgit(name_str) {
        return PathComponentError::HfsDotGit;
    }
    if opts.protect_ntfs && is_ntfs_dotgit(name_str) {
        return PathComponentError::NtfsDotGit;
    }
    PathComponentError::DotGit
}

fn scan_ntfs_backslash_dotgit(name: &str) -> bool {
    scan_ntfs_backslash_dot_special(name, is_ntfs_dotgit)
}

fn ntfs_backslash_dotdot_tail(after: &str) -> bool {
    after == ".." || after.starts_with("..\\") || after.starts_with("../")
}

fn scan_ntfs_backslash_dot_special(name: &str, mut pred: impl FnMut(&str) -> bool) -> bool {
    if !name.contains('\\') {
        return false;
    }
    let mut rest = name;
    while let Some(idx) = rest.find('\\') {
        let segment = &rest[..idx];
        if !segment.is_empty() && pred(segment) {
            return true;
        }
        rest = &rest[idx + 1..];
    }
    !rest.is_empty() && pred(rest)
}

fn has_dos_drive_prefix_bytes(name: &[u8]) -> bool {
    name.len() >= 2 && name[0].is_ascii_alphabetic() && name[1] == b':'
}

/// Win32 file-name rules for one component; `\` also separates segments there.
fn win32_component_error(name: &[u8]) -> Option<PathComponentError> {
    name.split(|&b| b == b'\\').find_map(win32_segment_error)
}

fn win32_segment_error(segment: &[u8]) -> Option<PathComponentError> {
    if is_win32_reserved_name(segment) {
        return Some(PathComponentError::NtfsReservedName);
    }
    if segment.contains(&b':') {
        return Some(PathComponentError::NtfsDriveOrStream);
    }
    if segment
        .iter()
        .any(|&b| b < 0x20 || matches!(b, b'<' | b'>' | b'"' | b'|' | b'?' | b'*'))
    {
        return Some(PathComponentError::NtfsInvalidChar);
    }
    let is_dot_or_dotdot = matches!(segment, b"." | b"..");
    if !is_dot_or_dotdot && matches!(segment.last(), Some(b' ' | b'.')) {
        return Some(PathComponentError::NtfsTrailingDotOrSpace);
    }
    None
}

/// A device name stays reserved when followed by spaces, an extension, or a stream suffix.
fn is_win32_reserved_name(segment: &[u8]) -> bool {
    let stem_end = segment
        .iter()
        .position(|&b| b == b'.' || b == b':')
        .unwrap_or(segment.len());
    let stem = segment[..stem_end].trim_ascii_end();
    const DEVICES: [&[u8]; 6] = [b"con", b"prn", b"aux", b"nul", b"conin$", b"conout$"];
    if DEVICES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
    {
        return true;
    }
    let (prefix, port) = stem.split_at(stem.len().min(3));
    let is_com = prefix.eq_ignore_ascii_case(b"com");
    let is_lpt = prefix.eq_ignore_ascii_case(b"lpt");
    match port {
        // Windows also reserves the superscript ports `¹`, `²` and `³` (UTF-8 encoded).
        [b'1'..=b'9'] | [0xc2, 0xb9 | 0xb2 | 0xb3] => is_com || is_lpt,
        [b'0'] => is_lpt,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: EntryMode = EntryMode::new(0o100644);
    const SYMLINK: EntryMode = EntryMode::new(0o120000);
    const FSCK: ProtectOptions = ProtectOptions::fsck();
    const OFF: ProtectOptions = ProtectOptions {
        protect_ntfs: false,
        protect_hfs: false,
    };
    const FSCK_KIND: PathValidationKind = PathValidationKind::Fsck;
    const WT_KIND: PathValidationKind = PathValidationKind::Worktree;

    #[test]
    fn empty_dot_and_dotdot_rejected() {
        assert_eq!(
            validate_component(b"", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::Empty
        );
        assert_eq!(
            validate_component(b".", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::Dot
        );
        assert_eq!(
            validate_component(b"..", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::DotDot
        );
    }

    #[test]
    fn slash_and_nul_in_component() {
        assert_eq!(
            validate_component(b"foo/bar", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::ContainsSlash
        );
        assert_eq!(
            validate_component(b"foo\0bar", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::ContainsNul
        );
    }

    #[test]
    fn ascii_dotgit_and_git_tilde_one() {
        for name in [b".git".as_slice(), b".GIT".as_slice()] {
            assert_eq!(
                validate_component(name, BLOB, OFF, FSCK_KIND).unwrap_err(),
                PathComponentError::DotGit
            );
        }
        for name in [b"git~1".as_slice(), b"GIT~1".as_slice()] {
            assert_eq!(
                validate_component(name, BLOB, FSCK, FSCK_KIND).unwrap_err(),
                PathComponentError::DotGit
            );
            assert!(validate_component(name, BLOB, OFF, FSCK_KIND).is_ok());
        }
        assert!(validate_component(b"git~2", BLOB, OFF, FSCK_KIND).is_ok());
    }

    #[test]
    fn hfs_dotgit_unicode_ignored_codepoints() {
        let name = ".gI\u{200c}T";
        assert_eq!(
            validate_component(name.as_bytes(), BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::HfsDotGit
        );
        assert!(validate_component(name.as_bytes(), BLOB, OFF, FSCK_KIND).is_ok());
    }

    #[test]
    fn ntfs_dotgit_trailing_dot_and_spaces() {
        assert_eq!(
            validate_component(b".git.", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::NtfsDotGit
        );
        assert_eq!(
            validate_component(b".git ", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::NtfsDotGit
        );
    }

    #[test]
    fn ntfs_git_tilde_one_with_trailing_spaces_or_periods() {
        for name in [
            b"GIT~1.".as_slice(),
            b"git~1 ",
            b"git~1...",
            b"git~1::$DATA",
        ] {
            for kind in [FSCK_KIND, WT_KIND] {
                assert_eq!(
                    validate_component(name, BLOB, FSCK, kind).unwrap_err(),
                    PathComponentError::NtfsDotGit,
                    "reject {name:?}"
                );
            }
            assert!(audit_tree_entry_name(name, FSCK).dotgit);
            if !cfg!(windows) {
                assert!(validate_component(name, BLOB, OFF, FSCK_KIND).is_ok());
                assert!(validate_component(name, BLOB, OFF, WT_KIND).is_ok());
            }
        }
        assert_eq!(
            validate_path(b"GIT~1./marker.txt", BLOB, FSCK, WT_KIND).unwrap_err(),
            PathComponentError::NtfsDotGit
        );
        assert!(validate_component(b"git~1.txt", BLOB, FSCK, WT_KIND).is_ok());
    }

    #[test]
    fn win32_reserved_device_names() {
        for name in [
            "aux",
            "AUX",
            "con",
            "Prn",
            "nul",
            "COM1",
            "com9",
            "lpt0",
            "LPT9",
            "CONIN$",
            "conout$",
            "COM1.txt",
            "aux.c",
            "nul .txt",
            "aux  ",
            "CON:stream",
            "COM¹",
            "lpt³.x",
            "a\\aux",
        ] {
            assert_eq!(
                win32_component_error(name.as_bytes()),
                Some(PathComponentError::NtfsReservedName),
                "reject {name:?}"
            );
        }
        for name in [
            "auxiliary",
            "con1",
            "COM0",
            "COM10",
            "LPT",
            "comx",
            "xaux",
            "aux_",
            "conin",
            "nul x",
            "CONOUT",
            "COM⁴",
            "auxa.txt",
        ] {
            assert_eq!(
                win32_component_error(name.as_bytes()),
                None,
                "accept {name:?}"
            );
        }
    }

    #[test]
    fn win32_trailing_dots_spaces_and_invalid_chars() {
        for name in ["trail.", "trail ", "a.. ", "x\\y.", "x.\\y"] {
            assert_eq!(
                win32_component_error(name.as_bytes()),
                Some(PathComponentError::NtfsTrailingDotOrSpace),
                "reject {name:?}"
            );
        }
        for name in ["a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\tb", "a\x1fb"] {
            assert_eq!(
                win32_component_error(name.as_bytes()),
                Some(PathComponentError::NtfsInvalidChar),
                "reject {name:?}"
            );
        }
        assert_eq!(
            win32_component_error(b"a:b"),
            Some(PathComponentError::NtfsDriveOrStream)
        );
        for name in [
            "file.txt",
            ".hidden",
            "a b",
            "..x",
            "x\\..\\y",
            "\u{e9}t\u{e9}",
        ] {
            assert_eq!(
                win32_component_error(name.as_bytes()),
                None,
                "accept {name:?}"
            );
        }
    }

    #[test]
    fn win32_rules_apply_to_worktree_on_windows_only() {
        for (name, err) in [
            (b"aux".as_slice(), PathComponentError::NtfsReservedName),
            (b"COM1.txt", PathComponentError::NtfsReservedName),
            (b"trail.", PathComponentError::NtfsTrailingDotOrSpace),
            (b"what?.txt", PathComponentError::NtfsInvalidChar),
            (b"a:b", PathComponentError::NtfsDriveOrStream),
        ] {
            assert!(validate_component(name, BLOB, OFF, WT_KIND).is_ok());
            if cfg!(windows) {
                assert_eq!(
                    validate_component(name, BLOB, FSCK, WT_KIND).unwrap_err(),
                    err,
                    "reject {name:?}"
                );
            } else {
                assert!(validate_component(name, BLOB, FSCK, WT_KIND).is_ok());
            }
            if name != b"a:b" {
                assert!(validate_component(name, BLOB, FSCK, FSCK_KIND).is_ok());
            }
        }
        assert_eq!(
            validate_component(b".git.", BLOB, FSCK, WT_KIND).unwrap_err(),
            PathComponentError::NtfsDotGit
        );
    }

    #[test]
    fn ntfs_backslash_dotgit_segment() {
        assert_eq!(
            validate_component(b"foo\\.git", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::NtfsBackslash
        );
        for name in [b".git\\config".as_slice(), b".git\\", b".GIT\\config"] {
            assert_eq!(
                validate_component(name, BLOB, FSCK, WT_KIND).unwrap_err(),
                PathComponentError::NtfsBackslash,
                "reject {name:?}"
            );
        }
        if cfg!(windows) {
            assert_eq!(
                validate_component(br"a\..\x", BLOB, FSCK, FSCK_KIND).unwrap_err(),
                PathComponentError::NtfsBackslash
            );
        } else {
            assert!(validate_component(br"a\..\x", BLOB, FSCK, FSCK_KIND).is_ok());
        }
        assert!(validate_component(br"a\..\x", BLOB, OFF, FSCK_KIND).is_ok());
    }

    #[test]
    fn ntfs_alternate_stream_dotgit() {
        assert_eq!(
            validate_component(b".git::$INDEX_ALLOCATION", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::NtfsDotGit
        );
    }

    #[test]
    fn ntfs_dos_drive_prefix_in_component() {
        if cfg!(windows) {
            assert_eq!(
                validate_component(b"C:foo", BLOB, FSCK, FSCK_KIND).unwrap_err(),
                PathComponentError::NtfsDriveOrStream
            );
        } else {
            assert!(validate_component(b"C:foo", BLOB, FSCK, FSCK_KIND).is_ok());
            assert!(validate_component(b"C:foo", BLOB, FSCK, WT_KIND).is_ok());
        }
    }

    #[test]
    fn issue_1037_multibyte_at_byte_six_no_panic() {
        for name in ["abcde日.txt", "ab日本.txt", "file0日本.txt"] {
            assert!(validate_component(name.as_bytes(), BLOB, FSCK, FSCK_KIND).is_ok());
            assert!(!audit_tree_entry_name(name.as_bytes(), FSCK).dotgit);
        }
    }

    #[test]
    fn symlink_dot_special_files() {
        assert_eq!(
            validate_component(b".gitmodules", SYMLINK, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotGitmodules
        );
        assert_eq!(
            validate_component(b".gitattributes", SYMLINK, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotGitattributes
        );
        assert_eq!(
            validate_component(b".gitignore", SYMLINK, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotGitignore
        );
        assert_eq!(
            validate_component(b".mailmap", SYMLINK, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotMailmap
        );
        assert!(validate_component(b".gitmodules", BLOB, FSCK, FSCK_KIND).is_ok());
        assert!(validate_component(b".gitattributes", SYMLINK, FSCK, WT_KIND).is_ok());
    }

    #[test]
    fn symlink_ascii_dot_special_banned_when_protect_flags_off() {
        assert_eq!(
            validate_component(b".gitmodules", SYMLINK, OFF, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotGitmodules
        );
        assert_eq!(
            validate_component(b".gitignore", SYMLINK, OFF, FSCK_KIND).unwrap_err(),
            PathComponentError::SymlinkedDotGitignore
        );
    }

    #[test]
    fn dos_drive_prefix_on_non_utf8_bytes() {
        if cfg!(windows) {
            assert_eq!(
                validate_component(b"C:\xff", BLOB, FSCK, FSCK_KIND).unwrap_err(),
                PathComponentError::NtfsDriveOrStream
            );
        } else {
            assert!(validate_component(b"C:\xff", BLOB, FSCK, FSCK_KIND).is_ok());
        }
    }

    #[test]
    fn validate_path_rejects_empty_slash_delimited_components() {
        for path in [
            b"/a".as_slice(),
            b"a/".as_slice(),
            b"a//b".as_slice(),
            b"//a".as_slice(),
        ] {
            assert_eq!(
                validate_path(path, BLOB, FSCK, FSCK_KIND).unwrap_err(),
                PathComponentError::Empty,
                "path {:?} should be rejected",
                path
            );
        }
        assert!(validate_path(b"a/b", BLOB, FSCK, FSCK_KIND).is_ok());
    }

    #[test]
    fn validate_path_splits_on_slash() {
        assert!(validate_path(b"dir/file.txt", BLOB, FSCK, FSCK_KIND).is_ok());
        assert_eq!(
            validate_path(b"dir/.git/file", BLOB, FSCK, FSCK_KIND).unwrap_err(),
            PathComponentError::DotGit
        );
    }

    #[test]
    fn protect_options_from_config_defaults() {
        let cfg = ConfigSet::new();
        let opts = ProtectOptions::from_config(&cfg);
        assert!(opts.protect_ntfs);
        assert_eq!(opts.protect_hfs, cfg!(target_os = "macos"));
    }

    #[test]
    fn audit_matches_validate_for_tree_cases() {
        let audit = audit_tree_entry_name(b".git", FSCK);
        assert!(audit.dotgit);
        assert_eq!(audit.first_error().unwrap_err(), PathComponentError::DotGit);
    }
}

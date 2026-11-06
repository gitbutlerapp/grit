//! Git-compatible path component validation (`verify_path` / tree fsck rules).
//!
//! Centralizes NTFS/HFS dot-file checks and tree-entry rules that previously
//! lived in separate internal modules. Embedders use this before writing index entries,
//! checking out paths, or building trees; [`ProtectOptions`] mirrors
//! `core.protectNTFS` and `core.protectHFS`.
//!
//! ```rust
//! use grit_lib::path_validate::{
//!     validate_path, EntryMode, PathValidationKind, ProtectOptions,
//! };
//!
//! let opts = ProtectOptions::default();
//! let blob = EntryMode::new(0o100644);
//! let kind = PathValidationKind::Worktree;
//! validate_path(b"src/lib.rs", blob, opts, kind).unwrap();
//! assert!(validate_path(b"GIT~1./hooks", blob, opts, kind).is_err());
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
        let protect_ntfs = config.bool_with_default("core.protectNTFS", true);
        let default_hfs = cfg!(target_os = "macos");
        let protect_hfs = config.bool_with_default("core.protectHFS", default_hfs);
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
    #[error("path component uses a forbidden NTFS drive or stream prefix")]
    NtfsDriveOrStream,
    /// Character forbidden on NTFS (`:*?"<>|` or a control character).
    #[error("path component contains a forbidden NTFS character")]
    NtfsIllegalCharacter,
    /// Trailing space or period on a component (except `.` and `..`).
    #[error("path component ends with a space or period")]
    NtfsTrailingSpaceOrPeriod,
    /// DOS device name (`CON`, `AUX`, `COM1`, …) in the component.
    #[error("path component contains a reserved DOS device name")]
    NtfsReservedDeviceName,
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
        validate_win32_path_segment(name)?;
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
    if !may_alias_dotgit(name) {
        return false;
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

/// Cheap pre-filter for [`component_is_dotgit_alias`]: NTFS spellings start with `.`, `g`
/// or `~`, HFS spellings with `.` or an ignorable (non-ASCII) code point, and anything else
/// can only spell `.git` in a `\`-separated segment.
fn may_alias_dotgit(name: &[u8]) -> bool {
    matches!(name.first(), Some(b'.' | b'g' | b'G' | b'~' | 0x80..)) || name.contains(&b'\\')
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

/// Windows `core.protectNTFS` rules for one tree/index component.
///
/// Tree entry names may embed `\` segment separators on NTFS; each piece is
/// validated the same way Git does for `verify_path` on Windows.
fn validate_win32_path_segment(path: &[u8]) -> Result<(), PathComponentError> {
    if path.is_empty() {
        return Ok(());
    }
    let mut start = 0usize;
    for (offset, &byte) in path.iter().enumerate() {
        if byte == b'\\' || byte == b'/' {
            validate_win32_path_piece(&path[start..offset])?;
            start = offset + 1;
        }
    }
    validate_win32_path_piece(&path[start..])
}

fn validate_win32_path_piece(piece: &[u8]) -> Result<(), PathComponentError> {
    if piece.is_empty() {
        return Ok(());
    }
    if piece.iter().any(|&b| win32_forbidden_path_byte(b)) {
        return Err(PathComponentError::NtfsIllegalCharacter);
    }
    if win32_piece_has_bad_trailing_space_or_dot(piece) {
        return Err(PathComponentError::NtfsTrailingSpaceOrPeriod);
    }
    if win32_piece_starts_with_reserved_device(piece) {
        return Err(PathComponentError::NtfsReservedDeviceName);
    }
    Ok(())
}

const WIN32_FORBIDDEN_BYTES: [u8; 7] = [b':', b'<', b'>', 0x22, b'|', b'?', b'*'];

fn win32_forbidden_path_byte(byte: u8) -> bool {
    WIN32_FORBIDDEN_BYTES.contains(&byte) || (byte > 0 && byte < 0x20)
}

/// Rejects trailing space or dot unless the whole piece is `.` or `..`.
fn win32_piece_has_bad_trailing_space_or_dot(piece: &[u8]) -> bool {
    if piece == b"." || piece == b".." {
        return false;
    }
    matches!(piece.last(), Some(b' ' | b'.'))
}

fn win32_ascii_prefix_eq(piece: &[u8], prefix: &[u8]) -> bool {
    piece.len() >= prefix.len()
        && piece
            .iter()
            .zip(prefix.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Length of a DOS device stem at the start of `piece`, if any.
fn win32_device_stem_len(piece: &[u8]) -> Option<usize> {
    const THREE: &[&[u8]] = &[b"aux", b"prn", b"nul"];
    for stem in THREE {
        if win32_ascii_prefix_eq(piece, stem) {
            return Some(stem.len());
        }
    }
    if win32_ascii_prefix_eq(piece, b"conin$") {
        return Some(6);
    }
    if win32_ascii_prefix_eq(piece, b"conout$") {
        return Some(7);
    }
    if piece.len() >= 3 && win32_ascii_prefix_eq(piece, b"con") {
        return Some(3);
    }
    if piece.len() >= 4
        && win32_ascii_prefix_eq(&piece[..3], b"com")
        && piece[3].is_ascii_digit()
        && (b'1'..=b'9').contains(&piece[3])
    {
        return Some(4);
    }
    if piece.len() >= 4 && win32_ascii_prefix_eq(&piece[..3], b"lpt") && piece[3].is_ascii_digit() {
        return Some(4);
    }
    None
}

fn win32_piece_starts_with_reserved_device(piece: &[u8]) -> bool {
    let Some(stem_len) = win32_device_stem_len(piece) else {
        return false;
    };
    let mut tail = stem_len;
    while tail < piece.len() && piece[tail] == b' ' {
        tail += 1;
    }
    match piece.get(tail) {
        None => true,
        Some(b'.' | b':' | b'/' | b'\\') => true,
        Some(_) => false,
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
                let err = validate_component(name, BLOB, FSCK, kind).unwrap_err();
                // On Windows the Win32 segment rules run first and report the trailing dot,
                // space or `:` instead.
                if !cfg!(windows) {
                    assert_eq!(err, PathComponentError::NtfsDotGit, "reject {name:?}");
                }
            }
            assert!(audit_tree_entry_name(name, FSCK).dotgit);
            if !cfg!(windows) {
                assert!(validate_component(name, BLOB, OFF, FSCK_KIND).is_ok());
                assert!(validate_component(name, BLOB, OFF, WT_KIND).is_ok());
            }
        }
        assert!(validate_path(b"GIT~1./marker.txt", BLOB, FSCK, WT_KIND).is_err());
        assert!(validate_component(b"git~1.txt", BLOB, FSCK, WT_KIND).is_ok());
    }

    #[test]
    fn dotgit_prefilter_admits_every_alias_spelling() {
        for name in [
            ".git".as_bytes(),
            b".GIT.",
            b"git~1",
            b"GIT~1 .",
            b"g7c2~123",
            b"~1234567",
            ".gI\u{200c}T".as_bytes(),
            "\u{200c}.git".as_bytes(),
            b"foo\\.git",
            b"x\\git~1",
        ] {
            assert!(may_alias_dotgit(name), "pre-filter must admit {name:?}");
            assert!(
                component_is_dotgit_alias(name, FSCK),
                "{name:?} is a .git alias"
            );
        }
        for name in [
            b"src".as_slice(),
            b"README.md",
            b"Cargo.toml",
            b"_git",
            b"1git~1",
        ] {
            assert!(!may_alias_dotgit(name), "pre-filter skips {name:?}");
        }
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

    /// Git `is_valid_win32_path` segment rules (issue #1699); runs on every host.
    #[test]
    fn issue_1699_win32_path_segment_matrix() {
        let illegal_char = |name: &[u8]| {
            assert_eq!(
                validate_win32_path_segment(name).unwrap_err(),
                PathComponentError::NtfsIllegalCharacter,
                "{name:?}"
            );
        };
        let trailing = |name: &[u8]| {
            assert_eq!(
                validate_win32_path_segment(name).unwrap_err(),
                PathComponentError::NtfsTrailingSpaceOrPeriod,
                "{name:?}"
            );
        };
        let reserved = |name: &[u8]| {
            assert_eq!(
                validate_win32_path_segment(name).unwrap_err(),
                PathComponentError::NtfsReservedDeviceName,
                "{name:?}"
            );
        };

        illegal_char(b".gitmodules::$DATA");
        illegal_char(b"hidden.txt:payload");
        illegal_char(b"evil:stream");
        trailing(b"README.");
        trailing(b"sp ");
        reserved(b"aux.txt");
        reserved(b"AUX");
        reserved(b"com1");
        reserved(b"CONIN$");
        reserved(b"lpt9");

        assert!(validate_win32_path_segment(b"README.md").is_ok());
        assert!(validate_win32_path_segment(b".gitmodules").is_ok());
        assert!(validate_win32_path_segment(b"hidden.txt").is_ok());

        // Each `\`-separated piece is validated independently (Bugbot #1718).
        assert_eq!(
            validate_win32_path_segment(b"legit\\aux.txt").unwrap_err(),
            PathComponentError::NtfsReservedDeviceName
        );
        assert_eq!(
            validate_win32_path_segment(b"legit\\sp ").unwrap_err(),
            PathComponentError::NtfsTrailingSpaceOrPeriod
        );
        assert!(validate_win32_path_segment(b"legit\\subdir").is_ok());
    }

    #[test]
    fn issue_1699_validate_component_on_windows() {
        if !cfg!(windows) {
            return;
        }
        let opts = FSCK;
        let err = |name: &[u8]| validate_component(name, BLOB, opts, WT_KIND).unwrap_err();
        assert_eq!(
            err(b".gitmodules::$DATA"),
            PathComponentError::NtfsIllegalCharacter
        );
        assert_eq!(
            err(b"README."),
            PathComponentError::NtfsTrailingSpaceOrPeriod
        );
        assert_eq!(err(b"aux.txt"), PathComponentError::NtfsReservedDeviceName);
    }
}

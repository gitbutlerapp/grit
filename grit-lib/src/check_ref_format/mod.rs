//! Ref-name validation — `git check-ref-format` rules.
//!
//! This module implements the same validation logic as
//! `git check_refname_format()` in `git/refs.c`, including the
//! `--allow-onelevel`, `--refspec-pattern`, and `--normalize` options.
//!
//! # Rules
//!
//! A ref name is valid when:
//!
//! 1. No path component begins with `.`
//! 2. No `..` anywhere
//! 3. No ASCII control characters (< 0x20 or DEL 0x7f)
//! 4. No space, `~`, `^`, `:`, `?`, `[`, `\`
//! 5. No trailing `/`
//! 6. No path component ends with `.lock`
//! 7. No `@{`
//! 8. Cannot be exactly `@`
//! 9. No consecutive slashes `//` (unless `--normalize` collapses them)
//! 10. No leading `/` (unless `--normalize` strips it)
//! 11. No trailing `.`
//! 12. Must have at least two slash-separated components (unless
//!     `--allow-onelevel`)

mod ref_name;

pub use ref_name::{RefName, RefNamePolicy, RefNameRef};

use std::borrow::Cow;

use crate::git_path::normalize_path_copy;

use thiserror::Error;

/// Errors returned by ref-name validation and [`RefName::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefNameError {
    /// The ref name is empty.
    #[error("ref name is empty")]
    Empty,
    /// The ref name is exactly `@`.
    #[error("ref name is a lone '@'")]
    LoneAt,
    /// A component starts with `.`.
    #[error("ref name component starts with '.'")]
    ComponentStartsDot,
    /// The ref name contains `..`.
    #[error("ref name contains '..'")]
    DoubleDot,
    /// An illegal character was found (control chars, space, `~`, `^`, `:`, `?`, `[`, `\\`).
    #[error("ref name contains an illegal character")]
    IllegalChar,
    /// The ref name contains `@{{`.
    #[error("ref name contains '@{{'")]
    AtBrace,
    /// The ref name contains `*` but `--refspec-pattern` was not set, or
    /// contains more than one `*` with `--refspec-pattern`.
    #[error("ref name contains invalid use of '*'")]
    InvalidWildcard,
    /// A path component ends with `.lock`.
    #[error("ref name component ends with '.lock'")]
    DotLock,
    /// The ref name ends with `/`.
    #[error("ref name ends with '/'")]
    TrailingSlash,
    /// The ref name starts with `/` (after normalization).
    #[error("ref name starts with '/'")]
    LeadingSlash,
    /// The ref name ends with `.`.
    #[error("ref name ends with '.'")]
    TrailingDot,
    /// The ref name has only one component and `--allow-onelevel` was not set.
    #[error("ref name has only one component (needs --allow-onelevel)")]
    OneLevel,
    /// The branch short name is reserved (e.g. `HEAD`).
    #[error("branch name is reserved")]
    ReservedBranchName,
    /// Branch short names must not begin with `-` (matches Git's `check_branch_ref`).
    #[error("branch name must not start with '-'")]
    BranchStartsWithDash,
    /// The ref name has zero-length components (consecutive slashes) that
    /// cannot be normalized away.
    #[error("ref name contains consecutive slashes")]
    ConsecutiveSlashes,
    /// [`RefNameRef::parse`] was called with normalization that would change the input.
    #[error("ref name requires normalization; use RefName::parse")]
    NormalizationRequired,
    /// The name is not safe to store as a ref.
    #[error("ref name is not storable")]
    NotStorable,
    /// The name is not safe to map under ref storage.
    #[error("ref name is not safe for the filesystem")]
    UnsafeForFilesystem,
    /// The name is not valid on a fetch/upload-pack advertisement.
    #[error("ref name is not a valid advertised ref")]
    NotAdvertisedFetchRef,
    /// The name would be rejected by receive-pack.
    #[error("ref name is not valid for receive-pack")]
    ReceivePackRefused,
}

/// Low-level format flags (`git check-ref-format` options).
#[derive(Debug, Clone, Default)]
pub(crate) struct RefFormatOptions {
    pub allow_onelevel: bool,
    pub refspec_pattern: bool,
    pub normalize: bool,
}

/// Validate `refname` according to Git ref-name rules.
///
/// Returns `Ok(normalized)` where `normalized` is:
/// - the ref name itself when `opts.normalize` is `false`, or
/// - the ref name with leading `/` stripped and consecutive slashes
///   collapsed when `opts.normalize` is `true`.
///
/// Returns `Err` when the ref name is invalid.
pub(crate) fn check_refname_format(
    refname: &str,
    opts: &RefFormatOptions,
) -> Result<String, RefNameError> {
    check_refname_format_borrowed(refname, opts).map(Cow::into_owned)
}

/// Validate a ref name without copying it unless normalization is requested.
///
/// Returns a borrowed `refname` or the normalized name, with the same errors as
/// [`check_refname_format`] for the supplied `opts`.
pub(crate) fn check_refname_format_borrowed<'a>(
    refname: &'a str,
    opts: &RefFormatOptions,
) -> Result<Cow<'a, str>, RefNameError> {
    if refname.is_empty() {
        return Err(RefNameError::Empty);
    }

    // Apply normalization (collapse leading/consecutive slashes) when requested.
    let normalized = if opts.normalize {
        Cow::Owned(collapse_slashes(refname))
    } else {
        Cow::Borrowed(refname)
    };

    let name: &str = &normalized;

    if name.is_empty() {
        return Err(RefNameError::Empty);
    }

    // Lone '@' is always invalid.
    if name == "@" {
        return Err(RefNameError::LoneAt);
    }

    // Leading '/' is invalid (even after normalization collapse_slashes strips
    // a leading slash, so if it's still here it means the whole name was just
    // slashes → empty after stripping, caught above).
    // Actually collapse_slashes keeps one leading slash if there is content
    // after it — in non-normalize mode we reject it directly.
    if !opts.normalize && name.starts_with('/') {
        return Err(RefNameError::LeadingSlash);
    }

    // Trailing '/' is always invalid (even after normalize it would be gone
    // because there's no component after it).
    if name.ends_with('/') {
        return Err(RefNameError::TrailingSlash);
    }

    // Trailing '.' is always invalid.
    if name.ends_with('.') {
        return Err(RefNameError::TrailingDot);
    }

    // Walk through the name byte-by-byte, tracking component starts.
    let bytes = name.as_bytes();
    let mut component_start = 0usize;
    let mut component_count = 0usize;
    let mut last = b'\0';
    let mut wildcard_used = false;

    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i];

        match ch {
            b'/' => {
                // End of a component.
                let comp_len = i - component_start;
                if comp_len == 0 {
                    // Consecutive or leading slash (shouldn't happen after
                    // normalization, but catch it in non-normalize mode).
                    return Err(RefNameError::ConsecutiveSlashes);
                }
                // Validate the finished component.
                validate_component(&bytes[component_start..i], &mut wildcard_used, opts)?;
                component_count += 1;
                component_start = i + 1;
                last = ch;
                i += 1;
                continue;
            }
            b'.' if last == b'.' => {
                return Err(RefNameError::DoubleDot);
            }
            b'{' if last == b'@' => {
                return Err(RefNameError::AtBrace);
            }
            b'*' => {
                if !opts.refspec_pattern {
                    return Err(RefNameError::InvalidWildcard);
                }
                if wildcard_used {
                    return Err(RefNameError::InvalidWildcard);
                }
                wildcard_used = true;
            }
            // Control characters (< 0x20 or DEL 0x7f) and forbidden chars.
            0x00..=0x1f | 0x7f | b' ' | b'~' | b'^' | b':' | b'?' | b'[' | b'\\' => {
                return Err(RefNameError::IllegalChar);
            }
            _ => {}
        }

        last = ch;
        i += 1;
    }

    // Validate the last component (from component_start to end).
    let last_comp = &bytes[component_start..];
    if last_comp.is_empty() {
        // Name ended with '/' — already checked above, but be safe.
        return Err(RefNameError::TrailingSlash);
    }
    validate_component(last_comp, &mut wildcard_used, opts)?;
    component_count += 1;

    // At least two components required unless --allow-onelevel.
    if !opts.allow_onelevel && component_count < 2 {
        return Err(RefNameError::OneLevel);
    }

    Ok(normalized)
}

/// Validate a single path component (the bytes between `/` separators, or the
/// entire name when there are no slashes).
///
/// Rules checked here:
/// - Must not start with `.`
/// - Must not end with `.lock`
fn validate_component(
    comp: &[u8],
    _wildcard_used: &mut bool,
    _opts: &RefFormatOptions,
) -> Result<(), RefNameError> {
    if comp.is_empty() {
        return Err(RefNameError::ConsecutiveSlashes);
    }

    // Component must not start with '.'.
    if comp[0] == b'.' {
        return Err(RefNameError::ComponentStartsDot);
    }

    // Component must not end with ".lock".
    const LOCK_SUFFIX: &[u8] = b".lock";
    if comp.len() >= LOCK_SUFFIX.len() && comp.ends_with(LOCK_SUFFIX) {
        return Err(RefNameError::DotLock);
    }

    Ok(())
}

/// Whether `refname` is safe to map to a path under ref storage.
///
/// Matches Git's `refname_is_safe()` in `refs.c`: refs under `refs/` must not
/// escape that prefix via `.` / `..` components; root-style names must be
/// uppercase ASCII letters and underscores only (e.g. `HEAD`).
#[must_use]
pub(crate) fn refname_is_safe(refname: &str) -> bool {
    if let Some(rest) = refname.strip_prefix("refs/") {
        if rest.is_empty() || rest.starts_with('/') || rest.ends_with('/') {
            return false;
        }
        return normalize_path_copy(rest)
            .ok()
            .is_some_and(|normalized| normalized == rest);
    }

    if refname.is_empty() {
        return false;
    }
    refname.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
}

pub(crate) use ref_name::{is_storable_ref_name, is_valid_fetch_advertised_ref};

/// Strip a leading `/` and collapse consecutive interior slashes to one.
///
/// This collapses runs of `/` the same way `git check-ref-format` does.
pub fn collapse_slashes(refname: &str) -> String {
    let mut result = String::with_capacity(refname.len());
    let mut prev = b'/';

    for ch in refname.bytes() {
        if prev == b'/' && ch == b'/' {
            // Skip consecutive slashes (including a leading one when prev
            // was initialized to '/').
            continue;
        }
        result.push(ch as char);
        prev = ch;
    }

    result
}

pub(crate) fn is_valid_advertised_symref_target(name: &str) -> bool {
    RefNameRef::parse(name, &RefNamePolicy::symref_target()).is_ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn validation_borrows_names_when_normalization_is_disabled() {
        let name = "refs/heads/topic";
        assert!(matches!(
            super::check_refname_format_borrowed(name, &super::RefFormatOptions::default()),
            Ok(std::borrow::Cow::Borrowed(value)) if value == name
        ));
        let opts = super::RefFormatOptions {
            normalize: true,
            ..Default::default()
        };
        assert!(matches!(
            super::check_refname_format_borrowed("/refs//heads/topic", &opts),
            Ok(std::borrow::Cow::Owned(value)) if value == name
        ));
    }

    use super::*;

    fn opts_default() -> RefFormatOptions {
        RefFormatOptions::default()
    }

    fn opts_onelevel() -> RefFormatOptions {
        RefFormatOptions {
            allow_onelevel: true,
            ..Default::default()
        }
    }

    fn opts_refspec() -> RefFormatOptions {
        RefFormatOptions {
            refspec_pattern: true,
            ..Default::default()
        }
    }

    fn opts_normalize() -> RefFormatOptions {
        RefFormatOptions {
            normalize: true,
            ..Default::default()
        }
    }

    fn valid(refname: &str, opts: &RefFormatOptions) {
        assert!(
            check_refname_format(refname, opts).is_ok(),
            "expected '{refname}' to be valid with opts={opts:?}"
        );
    }

    fn invalid(refname: &str, opts: &RefFormatOptions) {
        assert!(
            check_refname_format(refname, opts).is_err(),
            "expected '{refname}' to be invalid with opts={opts:?}"
        );
    }

    #[test]
    fn empty_is_invalid() {
        invalid("", &opts_default());
        invalid("", &opts_onelevel());
    }

    #[test]
    fn basic_valid() {
        valid("foo/bar/baz", &opts_default());
        valid("refs/heads/main", &opts_default());
    }

    #[test]
    fn one_level_requires_flag() {
        invalid("foo", &opts_default());
        valid("foo", &opts_onelevel());
    }

    #[test]
    fn double_dot_invalid() {
        invalid("heads/foo..bar", &opts_default());
    }

    #[test]
    fn trailing_dot_invalid() {
        invalid("refs/heads/foo.", &opts_default());
        invalid("heads/foo.", &opts_default());
    }

    #[test]
    fn component_starts_with_dot() {
        invalid("./foo", &opts_default());
        invalid(".refs/foo", &opts_default());
        invalid("foo/./bar", &opts_default());
    }

    #[test]
    fn dot_lock_invalid() {
        invalid("heads/foo.lock", &opts_default());
        invalid("foo.lock/bar", &opts_default());
    }

    #[test]
    fn at_brace_invalid() {
        invalid("heads/v@{ation", &opts_default());
    }

    #[test]
    fn lone_at_invalid() {
        invalid("@", &opts_default());
        invalid("@", &opts_onelevel());
    }

    #[test]
    fn wildcard_requires_flag() {
        invalid("foo/*", &opts_default());
        valid(
            "foo/*",
            &RefFormatOptions {
                refspec_pattern: true,
                allow_onelevel: false,
                normalize: false,
            },
        );
    }

    #[test]
    fn double_wildcard_invalid() {
        invalid("foo/*/*", &opts_refspec());
    }

    #[test]
    fn control_chars_invalid() {
        invalid("heads/foo\x01", &opts_default());
        invalid("heads/foo\x7f", &opts_default());
    }

    #[test]
    fn forbidden_chars_invalid() {
        invalid("heads/foo?bar", &opts_default());
        invalid("heads/foo bar", &opts_default());
        invalid("heads/foo~bar", &opts_default());
        invalid("heads/foo^bar", &opts_default());
        invalid("heads/foo:bar", &opts_default());
        invalid("heads/foo[bar", &opts_default());
        invalid("heads/foo\\bar", &opts_default());
    }

    #[test]
    fn normalize_collapses_slashes() {
        let result = check_refname_format("refs///heads/foo", &opts_normalize());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "refs/heads/foo");
    }

    #[test]
    fn normalize_strips_leading_slash() {
        let result = check_refname_format("/heads/foo", &opts_normalize());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "heads/foo");
    }

    #[test]
    fn leading_slash_without_normalize() {
        invalid("/heads/foo", &opts_default());
    }

    #[test]
    fn foo_dot_slash_bar_valid() {
        // "foo./bar" is valid — the dot is not at the start of a component
        // and doesn't form ".lock".
        valid("foo./bar", &opts_default());
    }

    #[test]
    fn utf8_allowed() {
        // Non-ASCII bytes that are valid UTF-8 are allowed.
        valid("heads/fu\u{00DF}", &opts_default());
    }

    #[test]
    fn refname_is_safe_rejects_traversal_under_refs() {
        assert!(!refname_is_safe("refs/heads/../../config"));
        assert!(!refname_is_safe("refs/foo/../../bar"));
        assert!(!refname_is_safe("refs/foo/../bar"));
        assert!(refname_is_safe("refs/heads/main"));
        assert!(refname_is_safe("HEAD"));
    }

    #[test]
    fn receive_pack_refname_ok_rejects_funny_refs() {
        use super::ref_name::receive_pack_refname_ok;
        assert!(!receive_pack_refname_ok("refs/heads/../../config", true));
        assert!(!receive_pack_refname_ok("refs/heads/../../config", false));
        assert!(!receive_pack_refname_ok("heads/main", false));
        assert!(receive_pack_refname_ok("refs/heads/main", false));
        assert!(receive_pack_refname_ok("refs/tags/v1", true));
    }

    #[test]
    fn fetch_advertised_ref_rejects_traversal_and_illegal() {
        assert!(is_valid_fetch_advertised_ref("refs/heads/main"));
        assert!(is_valid_fetch_advertised_ref("refs/tags/v1.0"));
        assert!(!is_valid_fetch_advertised_ref("refs/heads/../../../config"));
        assert!(!is_valid_fetch_advertised_ref("heads/main"));
        assert!(!is_valid_fetch_advertised_ref("refs/heads/foo..bar"));
    }

    #[test]
    fn symref_target_rejects_traversal() {
        assert!(is_valid_advertised_symref_target("refs/heads/main"));
        assert!(!is_valid_advertised_symref_target(
            "refs/heads/../../../config"
        ));
    }

    #[test]
    fn validate_branch_short_name_issue_927() {
        let invalid = ["bad name", "x..y", "a~b", "a.lock", "HEAD", "a:b", "a*b"];
        for name in invalid {
            assert!(
                RefName::branch(name).is_err(),
                "expected branch name {name:?} to be rejected"
            );
        }
        assert!(RefName::branch("main").is_ok());
        assert!(RefName::branch("feature/foo").is_ok());
        assert!(RefName::branch("@").is_ok());
        assert!(RefName::branch("-").is_err());
        assert!(RefName::branch("-topic").is_err());
    }

    #[test]
    fn validate_tag_short_name_issue_927() {
        let invalid = ["bad name", "a..b", "x.lock"];
        for name in invalid {
            assert!(
                RefName::tag(name).is_err(),
                "expected tag name {name:?} to be rejected"
            );
        }
        assert!(RefName::tag("v1.0").is_ok());
        assert!(RefName::tag("@").is_ok());
        assert!(RefName::tag("HEAD").is_ok());
    }
}

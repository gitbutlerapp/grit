//! Refspec parsing and validation — a port of `git/refspec.c`.
//!
//! A *refspec* describes how references map between a local and a remote
//! repository.  It has the general form `[+|^]<src>[:<dst>]`.  This module
//! parses a single refspec string and validates it according to the same rules
//! as Git's `parse_refspec()`, distinguishing fetch refspecs from push
//! refspecs (the two have slightly different validity rules).
//!
//! The primary entry points are [`parse_fetch_refspec`] and
//! [`parse_push_refspec`], which return [`RefspecItem`] on success or
//! [`RefspecError::Invalid`] when the refspec is malformed.  Callers that only
//! care about validity (for example loading `remote.<name>.fetch` /
//! `remote.<name>.push` config) can use [`valid_fetch_refspec`] and
//! [`valid_push_refspec`].

use crate::check_ref_format::{check_refname_format, RefNameOptions};

/// A parsed refspec item.
///
/// Holds a parsed refspec, capturing the
/// modifier flags and the source/destination sides of the mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefspecItem {
    /// The `+` modifier was present (force / non-fast-forward update).
    pub force: bool,
    /// The `^` modifier was present (negative refspec — exclusion).
    pub negative: bool,
    /// This refspec is the bare `:` (or `+:`) push refspec for matching refs.
    pub matching: bool,
    /// The refspec uses a `*` glob pattern.
    pub pattern: bool,
    /// The source side is an exact (full-length hex) object id.
    pub exact_sha1: bool,
    /// The source side (`<src>`), or `None` when absent.
    pub src: Option<String>,
    /// The destination side (`<dst>`), or `None` when no `:` was present.
    pub dst: Option<String>,
}

/// Error returned when a refspec cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefspecError {
    /// The refspec string is invalid.
    Invalid(String),
}

impl std::fmt::Display for RefspecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefspecError::Invalid(s) => write!(f, "invalid refspec '{s}'"),
        }
    }
}

impl std::error::Error for RefspecError {}

/// Length of a full SHA-1 hex object id.
const SHA1_HEXSZ: usize = 40;

/// Returns `true` when `s` is a string of exactly `SHA1_HEXSZ` hex digits.
fn is_exact_sha1_hex(s: &str) -> bool {
    s.len() == SHA1_HEXSZ && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validate `name` as a refname using the same flags as Git's
/// `check_refname_format` with `REFNAME_ALLOW_ONELEVEL` and, when `is_glob` is
/// set, `REFNAME_REFSPEC_PATTERN`.
///
/// Returns `true` when the name is well-formed.
fn refname_ok(name: &str, is_glob: bool) -> bool {
    let opts = RefNameOptions {
        allow_onelevel: true,
        refspec_pattern: is_glob,
        normalize: false,
    };
    check_refname_format(name, &opts).is_ok()
}

/// Parse a single refspec.  `fetch` selects fetch (`true`) vs push (`false`)
/// validity rules.
///
/// This is a direct port of `parse_refspec()` in `git/refspec.c`.
fn parse_refspec(refspec: &str, fetch: bool) -> Result<RefspecItem, RefspecError> {
    let bytes = refspec.as_bytes();
    let invalid = || RefspecError::Invalid(refspec.to_owned());

    let mut item = RefspecItem::default();
    let mut is_glob = false;

    // Leading modifier: '+' (force) or '^' (negative).
    let mut lhs_start = 0usize;
    if let Some(&first) = bytes.first() {
        if first == b'+' {
            item.force = true;
            lhs_start = 1;
        } else if first == b'^' {
            item.negative = true;
            lhs_start = 1;
        }
    }

    let lhs = &refspec[lhs_start..];

    // rhs points to the last ':' within lhs (strrchr).
    let colon_pos = lhs.rfind(':');

    // Negative refspecs only have one side.
    if item.negative && colon_pos.is_some() {
        return Err(invalid());
    }

    // Special case ":" (or "+:") as a push refspec for matching refs.
    // In C: rhs == lhs && rhs[1] == '\0' — i.e. the ':' is the first char of
    // lhs and is the only char.
    if !fetch && colon_pos == Some(0) && lhs.len() == 1 {
        item.matching = true;
        return Ok(item);
    }

    // Compute src (lhs) and dst (rhs) substrings.
    let (lhs_str, rhs_opt): (&str, Option<&str>) = match colon_pos {
        Some(pos) => (&lhs[..pos], Some(&lhs[pos + 1..])),
        None => (lhs, None),
    };

    if let Some(rhs) = rhs_opt {
        let rlen = rhs.len();
        is_glob = rlen >= 1 && rhs.contains('*');
        item.dst = Some(rhs.to_owned());
    } else {
        item.dst = None;
    }

    let llen = lhs_str.len();
    if llen >= 1 && lhs_str.contains('*') {
        // LHS has a '*'.
        if (rhs_opt.is_some() && !is_glob) || (rhs_opt.is_none() && !item.negative && fetch) {
            return Err(invalid());
        }
        is_glob = true;
    } else if rhs_opt.is_some() && is_glob {
        // RHS is a glob but LHS is not.
        return Err(invalid());
    }

    item.pattern = is_glob;
    if llen == 1 && lhs_str == "@" {
        item.src = Some("HEAD".to_owned());
    } else {
        item.src = Some(lhs_str.to_owned());
    }
    let src = item.src.as_deref().unwrap_or("");

    if item.negative {
        // Negative refspecs only have a LHS.
        if src.is_empty() {
            return Err(invalid()); // must not be empty
        } else if is_exact_sha1_hex(src) {
            return Err(invalid()); // cannot be exact sha1
        } else if refname_ok(src, is_glob) {
            // valid looking ref is ok
        } else {
            return Err(invalid());
        }
        return Ok(item);
    }

    if fetch {
        // LHS
        if src.is_empty() {
            // empty is ok; it means "HEAD"
        } else if is_exact_sha1_hex(src) {
            item.exact_sha1 = true; // ok
        } else if refname_ok(src, is_glob) {
            // valid looking ref is ok
        } else {
            return Err(invalid());
        }
        // RHS
        match item.dst.as_deref() {
            None | Some("") => {} // missing/empty: do not store
            Some(dst) => {
                if !refname_ok(dst, is_glob) {
                    return Err(invalid());
                }
            }
        }
    } else {
        // push
        // LHS
        if src.is_empty() {
            // empty is ok
        } else if is_glob {
            if !refname_ok(src, is_glob) {
                return Err(invalid());
            }
        } else {
            // anything goes, for now
        }
        // RHS
        match item.dst.as_deref() {
            None => {
                // missing is allowed, but LHS then must be a valid looking ref.
                if !refname_ok(src, is_glob) {
                    return Err(invalid());
                }
            }
            Some("") => {
                // empty is not allowed.
                return Err(invalid());
            }
            Some(dst) => {
                if !refname_ok(dst, is_glob) {
                    return Err(invalid());
                }
            }
        }
    }

    Ok(item)
}

/// Parse a fetch refspec, returning the parsed item or an error.
pub fn parse_fetch_refspec(refspec: &str) -> Result<RefspecItem, RefspecError> {
    parse_refspec(refspec, true)
}

/// Parse a push refspec, returning the parsed item or an error.
pub fn parse_push_refspec(refspec: &str) -> Result<RefspecItem, RefspecError> {
    parse_refspec(refspec, false)
}

/// Returns `true` when `refspec` is a valid fetch refspec.
pub fn valid_fetch_refspec(refspec: &str) -> bool {
    parse_refspec(refspec, true).is_ok()
}

/// Returns `true` when `refspec` is a valid push refspec.
pub fn valid_push_refspec(refspec: &str) -> bool {
    parse_refspec(refspec, false).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the cases in git's t5511-refspec.sh.

    fn fetch_valid(s: &str) {
        assert!(valid_fetch_refspec(s), "expected fetch '{s}' to be valid");
    }
    fn fetch_invalid(s: &str) {
        assert!(
            !valid_fetch_refspec(s),
            "expected fetch '{s}' to be invalid"
        );
    }
    fn push_valid(s: &str) {
        assert!(valid_push_refspec(s), "expected push '{s}' to be valid");
    }
    fn push_invalid(s: &str) {
        assert!(!valid_push_refspec(s), "expected push '{s}' to be invalid");
    }

    #[test]
    fn empty_and_colon() {
        push_invalid("");
        push_valid(":");
        push_invalid("::");
        push_valid("+:");
        fetch_valid("");
        fetch_valid(":");
        fetch_invalid("::");
    }

    #[test]
    fn glob_balance() {
        push_valid("refs/heads/*:refs/remotes/frotz/*");
        push_invalid("refs/heads/*:refs/remotes/frotz");
        push_invalid("refs/heads:refs/remotes/frotz/*");
        push_valid("refs/heads/main:refs/remotes/frotz/xyzzy");

        fetch_valid("refs/heads/*:refs/remotes/frotz/*");
        fetch_invalid("refs/heads/*:refs/remotes/frotz");
        fetch_invalid("refs/heads:refs/remotes/frotz/*");
        fetch_valid("refs/heads/main:refs/remotes/frotz/xyzzy");
        fetch_invalid("refs/heads/main::refs/remotes/frotz/xyzzy");
        fetch_invalid("refs/heads/maste :refs/remotes/frotz/xyzzy");
    }

    #[test]
    fn rev_expressions() {
        push_valid("main~1:refs/remotes/frotz/backup");
        fetch_invalid("main~1:refs/remotes/frotz/backup");
        push_valid("HEAD~4:refs/remotes/frotz/new");
        fetch_invalid("HEAD~4:refs/remotes/frotz/new");
    }

    #[test]
    fn bare_head_and_at() {
        push_valid("HEAD");
        fetch_valid("HEAD");
        push_valid("@");
        fetch_valid("@");
        push_invalid("refs/heads/ nitfol");
        fetch_invalid("refs/heads/ nitfol");
    }

    #[test]
    fn head_colon() {
        push_invalid("HEAD:");
        fetch_valid("HEAD:");
        push_invalid("refs/heads/ nitfol:");
        fetch_invalid("refs/heads/ nitfol:");
    }

    #[test]
    fn delete_specs() {
        push_valid(":refs/remotes/frotz/deleteme");
        fetch_valid(":refs/remotes/frotz/HEAD-to-me");
        push_invalid(":refs/remotes/frotz/delete me");
        fetch_invalid(":refs/remotes/frotz/HEAD to me");
    }

    #[test]
    fn star_placements() {
        fetch_valid("refs/heads/*/for-linus:refs/remotes/mine/*-blah");
        push_valid("refs/heads/*/for-linus:refs/remotes/mine/*-blah");
        fetch_valid("refs/heads*/for-linus:refs/remotes/mine/*");
        push_valid("refs/heads*/for-linus:refs/remotes/mine/*");
        fetch_invalid("refs/heads/*/*/for-linus:refs/remotes/mine/*");
        push_invalid("refs/heads/*/*/for-linus:refs/remotes/mine/*");
        fetch_invalid("refs/heads/*g*/for-linus:refs/remotes/mine/*");
        push_invalid("refs/heads/*g*/for-linus:refs/remotes/mine/*");
        fetch_valid("refs/heads/*/for-linus:refs/remotes/mine/*");
        push_valid("refs/heads/*/for-linus:refs/remotes/mine/*");
    }

    #[test]
    fn utf8_and_tab() {
        fetch_valid("refs/heads/\u{00C4}");
        fetch_invalid("refs/heads/\ttab");
    }
}

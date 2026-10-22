//! Ref-name version ordering for `version:refname` sort.
//!
//! Numeric runs in ref names follow Git's `versioncmp` / version sort (same rules as
//! `git for-each-ref --sort=version:refname`). Configured prerelease suffixes are
//! applied on raw bytes at the first mismatch, matching Git's `versionsort.suffix`.

use std::cmp::Ordering;

/// Compare two ref names for `version:refname` sort.
#[must_use]
pub fn compare_refname_versions(
    left: &str,
    right: &str,
    prerelease_suffixes: &[String],
) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    let lb = left.as_bytes();
    let rb = right.as_bytes();
    let off = mismatch_offset(lb, rb);
    if let Some(order) = prerelease_suffix_order(lb, rb, off, prerelease_suffixes) {
        return order;
    }
    compare_version_bytes(lb, rb)
}

/// Version-order comparison on raw bytes (portable; used on every target).
#[must_use]
pub(crate) fn compare_version_bytes(left: &[u8], right: &[u8]) -> Ordering {
    versioncmp_bytes(left, right)
}

/// Symbol class for the version-sort state machine (non-digit, non-zero digit, zero digit).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SymbolClass {
    Other = 0,
    Digit = 1,
    ZeroDigit = 2,
}

fn symbol_class(c: u8) -> SymbolClass {
    if c == b'0' {
        SymbolClass::ZeroDigit
    } else if c.is_ascii_digit() {
        SymbolClass::Digit
    } else {
        SymbolClass::Other
    }
}

/// Base comparison mode in Git's version sort automaton.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompareMode {
    Normal = 0,
    Integral = 1,
    Fraction = 2,
    ZeroRun = 3,
}

fn next_mode(mode: CompareMode, class: SymbolClass) -> CompareMode {
    match (mode, class) {
        (CompareMode::Normal, SymbolClass::Other) => CompareMode::Normal,
        (CompareMode::Normal, SymbolClass::Digit) => CompareMode::Integral,
        (CompareMode::Normal, SymbolClass::ZeroDigit) => CompareMode::ZeroRun,
        (CompareMode::Integral, SymbolClass::Other) => CompareMode::Normal,
        (CompareMode::Integral, SymbolClass::Digit | SymbolClass::ZeroDigit) => {
            CompareMode::Integral
        }
        (CompareMode::Fraction, SymbolClass::Other) => CompareMode::Normal,
        (CompareMode::Fraction, SymbolClass::Digit | SymbolClass::ZeroDigit) => {
            CompareMode::Fraction
        }
        (CompareMode::ZeroRun, SymbolClass::Other) => CompareMode::Normal,
        (CompareMode::ZeroRun, SymbolClass::Digit) => CompareMode::Fraction,
        (CompareMode::ZeroRun, SymbolClass::ZeroDigit) => CompareMode::ZeroRun,
    }
}

enum MismatchAction {
    ByCharDiff,
    NumericRunLength,
    PreferLess,
    PreferGreater,
}

fn mismatch_action(
    mode: CompareMode,
    left_class: SymbolClass,
    right_class: SymbolClass,
) -> MismatchAction {
    use CompareMode::{Fraction, Integral, Normal, ZeroRun};
    use MismatchAction::{ByCharDiff, NumericRunLength, PreferGreater, PreferLess};
    use SymbolClass::{Digit, Other, ZeroDigit};

    match (mode, left_class, right_class) {
        (Normal, Digit, Digit) => NumericRunLength,
        (Integral, Other, Digit | ZeroDigit) => PreferLess,
        (Integral, Digit | ZeroDigit, Other) => PreferGreater,
        (Integral, Digit | ZeroDigit, Digit | ZeroDigit) => NumericRunLength,
        (ZeroRun, Other, Digit | ZeroDigit) => PreferGreater,
        (ZeroRun, Digit | ZeroDigit, Other) => PreferLess,
        (Integral, Other, Other) => ByCharDiff,
        (Fraction, _, _) | (Normal, _, _) | (ZeroRun, _, _) => ByCharDiff,
    }
}

fn resolve_mismatch(
    mode: CompareMode,
    left_class: SymbolClass,
    right_class: SymbolClass,
    diff: i32,
    tail1: &[u8],
    tail2: &[u8],
) -> Ordering {
    match mismatch_action(mode, left_class, right_class) {
        MismatchAction::ByCharDiff => diff.cmp(&0),
        MismatchAction::PreferLess => Ordering::Less,
        MismatchAction::PreferGreater => Ordering::Greater,
        MismatchAction::NumericRunLength => compare_remaining_digit_runs(diff, tail1, tail2),
    }
}

fn versioncmp_bytes(s1: &[u8], s2: &[u8]) -> Ordering {
    if s1 == s2 {
        return Ordering::Equal;
    }

    let mut p1 = 0usize;
    let mut p2 = 0usize;
    let mut c1 = *s1.first().unwrap_or(&0);
    let mut c2 = *s2.first().unwrap_or(&0);
    if !s1.is_empty() {
        p1 += 1;
    }
    if !s2.is_empty() {
        p2 += 1;
    }

    let mut mode = CompareMode::Normal;
    let mut left_class = symbol_class(c1);

    loop {
        let diff = c1 as i32 - c2 as i32;
        if diff != 0 {
            return resolve_mismatch(
                mode,
                left_class,
                symbol_class(c2),
                diff,
                s1.get(p1..).unwrap_or(&[]),
                s2.get(p2..).unwrap_or(&[]),
            );
        }
        if c1 == 0 {
            return Ordering::Equal;
        }
        mode = next_mode(mode, left_class);
        c1 = *s1.get(p1).unwrap_or(&0);
        c2 = *s2.get(p2).unwrap_or(&0);
        if p1 < s1.len() {
            p1 += 1;
        }
        if p2 < s2.len() {
            p2 += 1;
        }
        left_class = symbol_class(c1);
    }
}

/// When two digit runs disagree mid-run, walk the rest of the runs (Git `LEN` case).
fn compare_remaining_digit_runs(diff: i32, mut tail1: &[u8], mut tail2: &[u8]) -> Ordering {
    loop {
        match tail1.first() {
            Some(&d) if d.is_ascii_digit() => {
                tail1 = &tail1[1..];
                match tail2.first() {
                    Some(&d2) if d2.is_ascii_digit() => tail2 = &tail2[1..],
                    _ => return Ordering::Greater,
                }
            }
            _ => break,
        }
    }
    if tail2.first().is_some_and(|d| d.is_ascii_digit()) {
        Ordering::Less
    } else {
        diff.cmp(&0)
    }
}

/// Byte index at the first difference between two NUL-terminated-style byte sequences.
fn mismatch_offset(left: &[u8], right: &[u8]) -> usize {
    let mut i1 = 0usize;
    let mut i2 = 0usize;
    let mut c1 = *left.get(i1).unwrap_or(&0);
    let mut c2 = *right.get(i2).unwrap_or(&0);
    if !left.is_empty() {
        i1 += 1;
    }
    if !right.is_empty() {
        i2 += 1;
    }
    while c1 == c2 {
        if c1 == 0 {
            return i1.saturating_sub(1);
        }
        c1 = *left.get(i1).unwrap_or(&0);
        c2 = *right.get(i2).unwrap_or(&0);
        if i1 < left.len() {
            i1 += 1;
        }
        if i2 < right.len() {
            i2 += 1;
        }
    }
    if c1 == 0 {
        i1
    } else if c2 == 0 {
        i2
    } else {
        i1.min(i2).saturating_sub(1)
    }
}

fn prerelease_suffix_order(
    left: &[u8],
    right: &[u8],
    near: usize,
    suffixes: &[String],
) -> Option<Ordering> {
    if suffixes.is_empty() {
        return None;
    }
    let left_hit = best_suffix_hit(left, suffixes, near);
    let right_hit = best_suffix_hit(right, suffixes, near);
    match (left_hit, right_hit) {
        (None, None) => None,
        (Some(l), Some(r)) if l == r => None,
        (Some(l), Some(r)) => Some(l.cmp(&r)),
        (Some(_), None) => Some(Ordering::Less),
        (None, Some(_)) => Some(Ordering::Greater),
    }
}

fn best_suffix_hit(haystack: &[u8], suffixes: &[String], near: usize) -> Option<usize> {
    let mut best_index: Option<usize> = None;
    let mut best_offset: usize = usize::MAX;
    let mut best_len: usize = 0;

    for (config_index, suffix) in suffixes.iter().enumerate() {
        let needle = suffix.as_bytes();
        if needle.is_empty() || haystack.len() < needle.len() {
            continue;
        }
        let start = near.saturating_sub(needle.len());
        let end = haystack.len() - needle.len();
        if end < start {
            continue;
        }
        let scan_end = if best_len < needle.len() {
            near
        } else {
            best_offset.saturating_sub(1)
        };
        for offset in start..=end.min(scan_end) {
            if haystack[offset..offset + needle.len()] == *needle {
                let better =
                    offset < best_offset || (offset == best_offset && needle.len() > best_len);
                if better {
                    best_index = Some(config_index);
                    best_offset = offset;
                    best_len = needle.len();
                }
                break;
            }
        }
    }
    best_index
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn utf8_ref_names_with_suffix_config_do_not_panic() {
        let suffixes = vec!["x".to_owned()];
        let _ = compare_refname_versions("refs/tags/vα1", "refs/tags/vα2", &suffixes);
    }

    #[test]
    fn numeric_chunks_sort_like_git_tags() {
        assert_eq!(
            compare_refname_versions("refs/tags/foo1.3", "refs/tags/foo1.10", &[]),
            Ordering::Less
        );
        assert_eq!(
            compare_refname_versions("refs/tags/foo1.10", "refs/tags/foo1.6", &[]),
            Ordering::Greater
        );
    }

    #[test]
    fn portable_compare_version_bytes_orders_runs() {
        assert_eq!(
            compare_version_bytes(b"foo1.10", b"foo1.9"),
            Ordering::Greater
        );
        assert_eq!(compare_version_bytes(b"1.10", b"1.9"), Ordering::Greater);
        assert_eq!(compare_version_bytes(b"a2", b"a10"), Ordering::Less);
    }

    #[test]
    fn prerelease_suffix_config() {
        let suffixes = vec!["-rc".to_owned()];
        assert_eq!(
            compare_refname_versions("v1.0", "v1.0-rc1", &suffixes),
            Ordering::Greater
        );
    }

    #[test]
    fn differing_digit_runs_are_antisymmetric() {
        let a = "refs/tags/a80";
        let b = "refs/tags/a90";
        let ab = compare_refname_versions(a, b, &[]);
        let ba = compare_refname_versions(b, a, &[]);
        assert_eq!(ab, Ordering::Less);
        assert_eq!(ba, Ordering::Greater);
        assert_eq!(ab, ba.reverse());
    }
}

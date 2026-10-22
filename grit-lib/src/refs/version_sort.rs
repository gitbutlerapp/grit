//! Ref-name version ordering for `version:refname` sort.
//!
//! Numeric runs in ref names are ordered like GNU `strverscmp` (Git's version sort).
//! Configured prerelease suffixes are applied on raw bytes at the first mismatch,
//! matching Git's `versionsort.suffix` behavior.

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

/// GNU `strverscmp` on raw bytes (portable; used on every target).
#[must_use]
pub(crate) fn compare_version_bytes(left: &[u8], right: &[u8]) -> Ordering {
    strverscmp_bytes(left, right)
}

fn strverscmp_bytes(s1: &[u8], s2: &[u8]) -> Ordering {
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

    let mut phase = char_phase_offset(c1);

    loop {
        let diff = c1 as i32 - c2 as i32;
        if diff != 0 {
            return resolve_mismatch(phase, c2, diff, s1, p1, s2, p2);
        }
        if c1 == 0 {
            return Ordering::Equal;
        }
        phase = NEXT_STATE[phase as usize];
        c1 = *s1.get(p1).unwrap_or(&0);
        c2 = *s2.get(p2).unwrap_or(&0);
        if p1 < s1.len() {
            p1 += 1;
        }
        if p2 < s2.len() {
            p2 += 1;
        }
        phase += char_phase_offset(c1);
    }
}

fn char_phase_offset(c: u8) -> i32 {
    (c == b'0') as i32 + c.is_ascii_digit() as i32
}

/// GNU `strverscmp` transition table (12 states).
const NEXT_STATE: [i32; 12] = [0, 3, 9, 0, 3, 3, 0, 6, 6, 0, 6, 9];

fn resolve_mismatch(
    phase: i32,
    c2: u8,
    diff: i32,
    s1: &[u8],
    p1: usize,
    s2: &[u8],
    p2: usize,
) -> Ordering {
    let idx = phase as usize * 3 + char_phase_offset(c2) as usize;
    const RESULT: [i8; 36] = [
        2, 2, 2, 2, 3, 2, 2, 2, 2, 2, -1, -1, 1, 3, 3, 1, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1,
        -1, 2, 2, -1, 2, 2,
    ];
    match RESULT[idx] {
        3 => {
            let mut q1 = p1;
            while s1.get(q1).is_some_and(u8::is_ascii_digit) {
                q1 += 1;
            }
            let mut q2 = p2;
            while s2.get(q2).is_some_and(u8::is_ascii_digit) {
                q2 += 1;
            }
            if q1 > p1 && !s2.get(p2).is_some_and(u8::is_ascii_digit) {
                return Ordering::Greater;
            }
            if s2.get(p2).is_some_and(u8::is_ascii_digit) {
                return Ordering::Less;
            }
            diff.cmp(&0)
        }
        2 => diff.cmp(&0),
        sign => {
            if sign > 0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
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
}

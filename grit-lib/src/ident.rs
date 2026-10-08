//! Git author/committer identity lines (`ident` in Git's `fsck.c` / `commit.c`).
//!
//! Parses `Name <email> <unix-timestamp> <+HHMM>` with the same edge cases as Git:
//! overflow, non-digit timestamps, and whitespace-only timestamps (sentinel handling).

use crate::git_date::tm::date_overflows;
use crate::objects::{HashAlgo, ObjectKind};

/// Parsed timestamp from a signature line for display and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTimestamp {
    /// Parsed seconds since Unix epoch (author/committer field); safe for `time_t` / display.
    Valid(i64),
    /// Unparsable, overflowing, or whitespace-only date field — Git uses a sentinel (epoch in
    /// headers, empty `%ad`, empty `%at` / `%ct` in format).
    Sentinel,
}

/// Successful parse of the trailing `<unix> <+HHMM>` portion of a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSignatureTimes {
    /// Seconds in the author/committer field (before tz offset).
    pub unix_seconds: i64,
    /// Signed offset in seconds (from `+HHMM` / `-HHMM`).
    pub tz_offset_secs: i64,
    /// Byte range of the `+HHMM` / `-HHMM` field in the original `ident` string.
    pub tz_hhmm_range: std::ops::Range<usize>,
}

/// Scan a decimal timestamp like Git's `parse_timestamp_from_buf` / `strtoumax`.
/// Returns `None` if there is no digit or more than 21 digits (Git uses a 24-byte buffer).
fn scan_decimal_timestamp(bytes: &[u8], mut i: usize) -> Option<(u128, usize)> {
    const MAX_DIGITS: usize = 21;
    let start = i;
    let mut count = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        count += 1;
        if count > MAX_DIGITS {
            return Some((u128::MAX, i));
        }
        i += 1;
    }
    if count == 0 {
        return None;
    }
    let s = std::str::from_utf8(&bytes[start..i]).ok()?;
    let v: u128 = s.parse().ok()?;
    Some((v, i))
}

/// After `>` and one required space, skip only ASCII spaces and horizontal tabs (Git `fsck_ident`).
fn skip_fsck_date_leading_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

fn parse_tz_hhmm_offset(offset: &str) -> Option<i64> {
    let b = offset.as_bytes();
    if b.len() < 5 {
        return None;
    }
    if !(b[0] == b'+' || b[0] == b'-') {
        return None;
    }
    let sign = if b[0] == b'-' { -1i64 } else { 1i64 };
    let hours: i64 = std::str::from_utf8(&b[1..3]).ok()?.parse().ok()?;
    let minutes: i64 = std::str::from_utf8(&b[3..5]).ok()?.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Parse `<unix> <+HHMM>` after the closing `>` of the email (Git commit author/committer line).
#[must_use]
pub fn parse_signature_times(ident: &str) -> Option<ParsedSignatureTimes> {
    match parse_signature_tail(ident)? {
        SignatureTail::Valid(p) => Some(p),
        SignatureTail::Overflow | SignatureTail::NonNumeric => None,
    }
}

/// Distinguishes a non-numeric date field from a numeric field that fails Git's overflow rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureTail {
    /// Well-formed timestamp and timezone.
    Valid(ParsedSignatureTimes),
    /// Digits and timezone present, but the number does not fit Git's `date_overflows` rules.
    /// Default `%ad` shows the Unix epoch with `+0000` (t4212).
    Overflow,
    /// No leading digit after `>` (e.g. `totally_bogus`): `%ad` is empty, headers use epoch `+0000`.
    NonNumeric,
}

/// Parse the date/time tail like [`parse_signature_times`], but preserve overflow vs non-numeric.
#[must_use]
pub fn parse_signature_tail(ident: &str) -> Option<SignatureTail> {
    let bytes = ident.as_bytes();
    let gt = ident.rfind('>')?;
    let mut i = skip_fsck_date_leading_ws(bytes, gt + 1);
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return Some(SignatureTail::NonNumeric);
    }
    let (raw, after_digits) = scan_decimal_timestamp(bytes, i)?;
    if after_digits >= bytes.len() || bytes[after_digits] != b' ' {
        return None;
    }
    i = after_digits + 1;
    if i + 5 > bytes.len() {
        return None;
    }
    let tz_slice = ident.get(i..i + 5)?;
    let tz_offset_secs = parse_tz_hhmm_offset(tz_slice)?;
    let tz_hhmm_range = i..i + 5;
    if raw == u128::MAX || raw > u64::MAX as u128 || date_overflows(raw as u64) {
        return Some(SignatureTail::Overflow);
    }
    let unix_seconds = i64::try_from(raw).ok()?;
    Some(SignatureTail::Valid(ParsedSignatureTimes {
        unix_seconds,
        tz_offset_secs,
        tz_hhmm_range,
    }))
}

/// Classify the date field for pretty-print / `%at` (t4212 whitespace commits).
pub fn signature_timestamp_for_pretty(ident: &str) -> SignatureTimestamp {
    match parse_signature_times(ident) {
        Some(p) => SignatureTimestamp::Valid(p.unix_seconds),
        None => SignatureTimestamp::Sentinel,
    }
}

/// Unix timestamp for `--until` / `--since` filtering (committer), matching Git's `parse_date`.
/// Sentinel timestamps (whitespace dates, overflow, etc.) behave like `0` for cutoff comparisons
/// (see t4212 `rev-list --until`).
#[must_use]
pub fn committer_timestamp_for_until_filter(ident: &str) -> i64 {
    match signature_timestamp_for_pretty(ident) {
        SignatureTimestamp::Valid(ts) => ts,
        SignatureTimestamp::Sentinel => 0,
    }
}

/// Raw unix seconds as i64 for `%at` when valid; `None` when Git would print nothing.
#[must_use]
pub fn timestamp_for_at_ct(ts: SignatureTimestamp) -> Option<i64> {
    match ts {
        SignatureTimestamp::Valid(v) => Some(v),
        SignatureTimestamp::Sentinel => None,
    }
}

/// First fsck error for commit headers (tree/parents/author/committer), or `Ok`.
///
/// Returns a [`FsckError::report_line`] string (`msg-id: detail`); compare msg-ids to Git, not detail text.
pub fn fsck_commit_idents(data: &[u8], hash_algo: HashAlgo) -> Result<(), String> {
    crate::fsck_standalone::fsck_object(
        ObjectKind::Commit,
        data,
        crate::fsck_standalone::FsckObjectOptions::new(hash_algo),
    )
    .map_err(|e| e.report_line())
}

/// Committer seconds for ordering (`rev-list --date-order`, etc.): unknown/corrupt → `0`.
#[must_use]
pub fn committer_unix_seconds_for_ordering(ident: &str) -> i64 {
    match signature_timestamp_for_pretty(ident) {
        SignatureTimestamp::Valid(ts) => ts,
        SignatureTimestamp::Sentinel => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_numeric_author_date_is_non_numeric_tail() {
        let ident = "A <e@x> totally_bogus -0700";
        assert!(matches!(
            parse_signature_tail(ident),
            Some(SignatureTail::NonNumeric)
        ));
    }

    #[test]
    fn u128_max_digit_count_is_overflow_tail() {
        let ident = "A <e@x> 18446744073709551617 -0700";
        assert!(matches!(
            parse_signature_tail(ident),
            Some(SignatureTail::Overflow)
        ));
    }
}

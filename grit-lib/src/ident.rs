//! Git author/committer identity lines (`ident` in Git's `fsck.c` / `commit.c`).
//!
//! Parses `Name <email> <unix-timestamp> <+HHMM>` with the same edge cases as Git:
//! overflow, non-digit timestamps, and whitespace-only timestamps (sentinel handling).
//!
//! ```rust
//! use grit_lib::ident::parse_author_arg;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let contact = parse_author_arg("Ada Lovelace <ada@example.com>")?;
//! assert_eq!(contact.name, "Ada Lovelace");
//! assert_eq!(contact.email, "ada@example.com");
//! # Ok(())
//! # }
//! ```

use crate::commit::assemble_identity;
use crate::git_date::tm::date_overflows;
use crate::objects::{HashAlgo, ObjectKind};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// Parsed timestamp from a signature line for display and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTimestamp {
    /// Parsed seconds since Unix epoch (author/committer field); safe for `time_t` / display.
    Valid(i64),
    /// Unparsable, overflowing, or whitespace-only date field — Git uses a sentinel (epoch in
    /// headers, empty `%ad`, empty `%at` / `%ct` in format).
    Sentinel,
}

/// Parsed author or committer identity from a full header or reflog identity line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Display name (may be empty).
    pub name: String,
    /// Email address inside `<…>`.
    pub email: String,
    /// Timestamp with the recorded `±HHMM` offset preserved.
    pub time: OffsetDateTime,
}

impl Signature {
    /// Format [`Self::time`] as RFC 3339 (includes the original offset).
    #[must_use]
    pub fn format_rfc3339(&self) -> Option<String> {
        self.time.format(&Rfc3339).ok()
    }
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
    parse_signature_tail_bytes(ident.as_bytes())
}

/// Byte-oriented tail parse: identity lines may contain non-UTF-8 names while the
/// `<unix> <±HHMM>` suffix stays ASCII (Git encoding headers).
#[must_use]
pub fn parse_signature_tail_bytes(bytes: &[u8]) -> Option<SignatureTail> {
    let gt = bytes.iter().rposition(|&b| b == b'>')?;
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
    let tz_slice = std::str::from_utf8(&bytes[i..i + 5]).ok()?;
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

/// Parse `Name <email> <unix> <±HHMM>` into a [`Signature`].
///
/// Returns [`None`] when the date field is missing, malformed, overflowed, or non-numeric
/// (no sentinel epoch is synthesized).
#[must_use]
pub fn parse_signature(ident: &str) -> Option<Signature> {
    let ParsedSignatureTimes {
        unix_seconds,
        tz_offset_secs,
        ..
    } = match parse_signature_tail(ident)? {
        SignatureTail::Valid(p) => p,
        SignatureTail::Overflow | SignatureTail::NonNumeric => return None,
    };
    let (name, email) = parse_identity_name_email(ident)?;
    let offset_secs = i32::try_from(tz_offset_secs).ok()?;
    let offset = UtcOffset::from_whole_seconds(offset_secs).ok()?;
    let utc = OffsetDateTime::from_unix_timestamp(unix_seconds).ok()?;
    Some(Signature {
        name,
        email,
        time: utc.to_offset(offset),
    })
}

fn parse_identity_name_email(ident: &str) -> Option<(String, String)> {
    let lt = ident.find('<')?;
    let name = ident[..lt].trim_end().to_owned();
    let rest = &ident[lt + 1..];
    let gt = rest.find('>')?;
    let email = rest[..gt].trim().to_owned();
    Some((name, email))
}

/// Classify the date field for pretty-print / `%at` (t4212 whitespace commits).
pub fn signature_timestamp_for_pretty(ident: &str) -> SignatureTimestamp {
    signature_timestamp_for_pretty_bytes(ident.as_bytes())
}

/// Like [`signature_timestamp_for_pretty`] on raw identity bytes (non-UTF-8 names allowed).
#[must_use]
pub fn signature_timestamp_for_pretty_bytes(bytes: &[u8]) -> SignatureTimestamp {
    match parse_signature_tail_bytes(bytes) {
        Some(SignatureTail::Valid(p)) => SignatureTimestamp::Valid(p.unix_seconds),
        Some(SignatureTail::Overflow | SignatureTail::NonNumeric) | None => {
            SignatureTimestamp::Sentinel
        }
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
/// Returns a [`crate::fsck_standalone::FsckError::report_line`] string (`msg-id: detail`); compare msg-ids to Git, not detail text.
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

/// Like [`committer_unix_seconds_for_ordering`] on raw identity bytes (ASCII commit headers).
#[must_use]
pub fn committer_unix_seconds_for_ordering_bytes(ident: &[u8]) -> i64 {
    match signature_timestamp_for_pretty_bytes(ident) {
        SignatureTimestamp::Valid(ts) => ts,
        SignatureTimestamp::Sentinel => 0,
    }
}

/// Like [`committer_timestamp_for_until_filter`] on raw identity bytes (ASCII commit headers).
#[must_use]
pub fn committer_timestamp_for_until_filter_bytes(ident: &[u8]) -> i64 {
    match signature_timestamp_for_pretty_bytes(ident) {
        SignatureTimestamp::Valid(ts) => ts,
        SignatureTimestamp::Sentinel => 0,
    }
}

/// Name and email from a `git commit --author`-style argument (`Name <email>` or bare email).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorContact {
    /// Display name (may be empty when the argument is a bare email address).
    pub name: String,
    /// Email address inside `<…>`.
    pub email: String,
}

/// Failure to parse [`parse_author_arg`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorArgError {
    /// The string is not a recognized author form.
    #[error("invalid author: expected 'Name <email>' or a bare email address")]
    InvalidFormat,
    /// Angle brackets present but the email portion is empty.
    #[error("invalid author: empty email")]
    EmptyEmail,
    /// Non-whitespace remains after the closing `>`.
    #[error("invalid author: trailing text after '>'")]
    TrailingText,
}

/// Parse `git commit --author` values: `Name <email>` or a bare email (no whitespace).
///
/// # Errors
///
/// Returns [`AuthorArgError`] when the string cannot be interpreted as an author.
pub fn parse_author_arg(raw: &str) -> Result<AuthorContact, AuthorArgError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(AuthorArgError::InvalidFormat);
    }
    if s.contains('\n') || s.contains('\r') {
        return Err(AuthorArgError::InvalidFormat);
    }
    if let Some(lt) = s.find('<') {
        let rest = &s[lt + 1..];
        let gt = rest.find('>').ok_or(AuthorArgError::InvalidFormat)?;
        let email = &rest[..gt];
        if email.is_empty() {
            return Err(AuthorArgError::EmptyEmail);
        }
        let after = rest[gt + 1..].trim();
        if !after.is_empty() {
            return Err(AuthorArgError::TrailingText);
        }
        let name = s[..lt].trim_end_matches(|c: char| c.is_ascii_whitespace());
        return Ok(AuthorContact {
            name: name.to_owned(),
            email: email.to_owned(),
        });
    }
    if s.contains('@') && !s.chars().any(char::is_whitespace) {
        return Ok(AuthorContact {
            name: String::new(),
            email: s.to_owned(),
        });
    }
    Err(AuthorArgError::InvalidFormat)
}

/// Build a full author/committer header line from a parsed contact and timestamp inputs.
#[must_use]
pub fn author_identity_line(
    contact: &AuthorContact,
    date_override: Option<&str>,
    now: OffsetDateTime,
) -> String {
    assemble_identity(&contact.name, &contact.email, date_override, now)
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

    #[test]
    fn parse_author_arg_name_and_email() {
        let c = parse_author_arg("A U Thor <author@example.com>").expect("parse");
        assert_eq!(c.name, "A U Thor");
        assert_eq!(c.email, "author@example.com");
    }

    #[test]
    fn parse_author_arg_bare_email() {
        let c = parse_author_arg("nobody@example.com").expect("parse");
        assert!(c.name.is_empty());
        assert_eq!(c.email, "nobody@example.com");
    }

    #[test]
    fn parse_author_arg_rejects_empty_email_in_brackets() {
        assert_eq!(parse_author_arg("Name <>"), Err(AuthorArgError::EmptyEmail));
    }

    #[test]
    fn parse_author_arg_rejects_trailing_garbage() {
        assert_eq!(
            parse_author_arg("A <a@b.com> extra"),
            Err(AuthorArgError::TrailingText)
        );
    }

    #[test]
    fn parse_author_arg_rejects_embedded_newline() {
        assert_eq!(
            parse_author_arg("Bad\nInjected <x@y.example>"),
            Err(AuthorArgError::InvalidFormat)
        );
    }

    #[test]
    fn parse_signature_preserves_positive_offset() {
        let ident = "Ada Lovelace <ada@example.com> 1700000000 +0530";
        let sig = parse_signature(ident).expect("parse");
        assert_eq!(sig.name, "Ada Lovelace");
        assert_eq!(sig.email, "ada@example.com");
        assert_eq!(
            sig.format_rfc3339().as_deref(),
            Some("2023-11-15T03:43:20+05:30")
        );
    }

    #[test]
    fn parse_signature_preserves_negative_offset() {
        let ident = "Ada Lovelace <ada@example.com> 1700000000 -0800";
        let sig = parse_signature(ident).expect("parse");
        assert_eq!(
            sig.format_rfc3339().as_deref(),
            Some("2023-11-14T14:13:20-08:00")
        );
    }

    #[test]
    fn parse_signature_malformed_identity_is_none() {
        assert!(parse_signature("A <e@x> totally_bogus -0700").is_none());
        assert!(parse_signature("not an identity").is_none());
    }

    #[test]
    fn ordering_timestamp_from_non_utf8_identity_bytes() {
        let ident = b"Lat\xEDn Dev <e@x> 1700000000 +0000";
        assert_eq!(
            committer_unix_seconds_for_ordering_bytes(ident),
            1_700_000_000
        );
        assert_eq!(
            committer_timestamp_for_until_filter_bytes(ident),
            1_700_000_000
        );
    }
}

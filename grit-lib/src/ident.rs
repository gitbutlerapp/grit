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

use crate::commit::{assemble_identity, format_git_timestamp};
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
    /// Build a signature for writing into a commit or tag.
    ///
    /// # Parameters
    ///
    /// - `name`: display name (may be empty).
    /// - `email`: address written inside `<…>` (may be empty).
    /// - `time`: timestamp; its UTC offset becomes the `±HHMM` field.
    ///
    /// # Errors
    ///
    /// See [`Self::validate`].
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::ident::Signature;
    /// use time::{OffsetDateTime, UtcOffset};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let time = OffsetDateTime::from_unix_timestamp(1_700_000_000)?
    ///     .to_offset(UtcOffset::from_hms(-8, 0, 0)?);
    /// let sig = Signature::new("Ada Lovelace", "ada@example.com", time)?;
    /// assert_eq!(sig.header_line(), "Ada Lovelace <ada@example.com> 1700000000 -0800");
    /// assert!(Signature::new("Ada <x>", "ada@example.com", time).is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        time: OffsetDateTime,
    ) -> Result<Self, SignatureError> {
        let signature = Self {
            name: name.into(),
            email: email.into(),
            time,
        };
        signature.validate()?;
        Ok(signature)
    }

    /// Check that [`Self::header_line`] yields an identity line `git fsck` accepts.
    ///
    /// # Errors
    ///
    /// - [`SignatureError::ForbiddenCharacter`] when the name or email contains `<`, `>`,
    ///   or a control character other than tab.
    /// - [`SignatureError::BeforeEpoch`] when the time is before 1970-01-01 UTC.
    /// - [`SignatureError::SubMinuteOffset`] when the UTC offset has a seconds component,
    ///   which the `±HHMM` field cannot represent.
    pub fn validate(&self) -> Result<(), SignatureError> {
        for (field, value) in [
            (SignatureField::Name, &self.name),
            (SignatureField::Email, &self.email),
        ] {
            if let Some(ch) = value
                .chars()
                .find(|&c| c == '<' || c == '>' || (c.is_ascii_control() && c != '\t'))
            {
                return Err(SignatureError::ForbiddenCharacter { field, ch });
            }
        }
        if self.time.unix_timestamp() < 0 {
            return Err(SignatureError::BeforeEpoch);
        }
        if self.time.offset().seconds_past_minute() != 0 {
            return Err(SignatureError::SubMinuteOffset);
        }
        Ok(())
    }

    /// Render as a header payload: `Name <email> <unix-seconds> <±HHMM>`.
    ///
    /// The result is what follows `author ` / `committer ` / `tagger ` in an object.
    /// It is only fsck-clean when [`Self::validate`] succeeds.
    #[must_use]
    pub fn header_line(&self) -> String {
        format!(
            "{} <{}> {}",
            self.name,
            self.email,
            format_git_timestamp(self.time)
        )
    }

    /// Format [`Self::time`] as RFC 3339 (includes the original offset).
    #[must_use]
    pub fn format_rfc3339(&self) -> Option<String> {
        self.time.format(&Rfc3339).ok()
    }
}

/// Which part of a [`Signature`] a [`SignatureError`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureField {
    /// [`Signature::name`].
    Name,
    /// [`Signature::email`].
    Email,
}

impl std::fmt::Display for SignatureField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Name => "name",
            Self::Email => "email",
        })
    }
}

/// Why a [`Signature`] cannot be written into an object (see [`Signature::validate`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignatureError {
    /// The name or email contains a character that would break the identity line.
    #[error("signature {field} contains forbidden character {ch:?}")]
    ForbiddenCharacter {
        /// Field holding the character.
        field: SignatureField,
        /// The first forbidden character.
        ch: char,
    },
    /// Git timestamps are unsigned seconds since the Unix epoch.
    #[error("signature time is before the Unix epoch")]
    BeforeEpoch,
    /// The UTC offset is not a whole number of minutes.
    #[error("signature UTC offset is not a whole number of minutes")]
    SubMinuteOffset,
    /// The text is not a `Name <email> <unix-seconds> <±HHMM>` identity line.
    #[error("not an identity line of the form 'Name <email> <unix-seconds> <+HHMM>'")]
    Malformed,
}

impl std::str::FromStr for Signature {
    type Err = SignatureError;

    /// Parse a full header identity line (`Name <email> <unix-seconds> <±HHMM>`).
    ///
    /// # Errors
    ///
    /// [`SignatureError::Malformed`] when the line does not have exactly one `<email>` or has a
    /// missing, non-numeric, or overflowing date; otherwise the [`Signature::validate`] errors.
    fn from_str(line: &str) -> Result<Self, Self::Err> {
        if line.matches('<').count() != 1 || line.matches('>').count() != 1 {
            return Err(SignatureError::Malformed);
        }
        let signature = parse_signature(line).ok_or(SignatureError::Malformed)?;
        signature.validate()?;
        Ok(signature)
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
    let Ok(ident) = std::str::from_utf8(ident) else {
        return 0;
    };
    committer_unix_seconds_for_ordering(ident)
}

/// Like [`committer_timestamp_for_until_filter`] on raw identity bytes (ASCII commit headers).
#[must_use]
pub fn committer_timestamp_for_until_filter_bytes(ident: &[u8]) -> i64 {
    let Ok(ident) = std::str::from_utf8(ident) else {
        return 0;
    };
    committer_timestamp_for_until_filter(ident)
}

/// Name and email from a `git commit --author`-style argument (`Name <email>` or bare email).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorContact {
    /// Display name (may be empty when the argument is a bare email address).
    pub name: String,
    /// Email address inside `<…>`.
    pub email: String,
}

impl AuthorContact {
    /// Stamp this contact with a timestamp, giving a [`Signature`].
    ///
    /// # Parameters
    ///
    /// - `date_override`: a `GIT_AUTHOR_DATE`-style date (`@<unix>`, `<unix> <±HHMM>`, ISO 8601,
    ///   RFC 2822, …); when `None`, `now` is used.
    /// - `now`: the current time; its UTC offset is used for `@<unix>` overrides.
    ///
    /// # Errors
    ///
    /// [`SignatureError::Malformed`] when `date_override` is not a date Git accepts, and the
    /// [`Signature::validate`] errors for the name and email.
    pub fn signature(
        &self,
        date_override: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Signature, SignatureError> {
        let line = author_identity_line(self, date_override, now);
        let time = parse_signature(&line)
            .ok_or(SignatureError::Malformed)?
            .time;
        Signature::new(self.name.clone(), self.email.clone(), time)
    }
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

/// Failure to normalize a commit author or committer string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitIdentityError {
    /// Newlines, carriage returns, or other C0 controls (except tab) in the input.
    #[error("invalid identity: embedded control character")]
    EmbeddedControl,
    /// The string is not a contact form, bare email, or full header line with timestamp.
    #[error("invalid identity: expected 'Name <email>', a bare email, or a full header line with timestamp")]
    Unrecognized,
    /// [`parse_author_arg`] failure for contact-style input.
    #[error(transparent)]
    Author(#[from] AuthorArgError),
}

fn identity_contains_embedded_control(raw: &str) -> bool {
    raw.bytes()
        .any(|b| b == b'\n' || b == b'\r' || (b < 0x20 && b != b'\t'))
}

/// Replace the `<unix> <±HHMM>` suffix on a full or contact-style identity line.
#[must_use]
pub fn replace_identity_date_suffix(identity: &str, date_suffix: &str) -> String {
    let suffix = date_suffix.trim();
    if let Some((name, email)) = parse_identity_name_email(identity) {
        if email.is_empty() && !name.is_empty() && name.contains('@') {
            return format!("{} {suffix}", name.trim());
        }
        if email.is_empty() {
            return format!("{} {suffix}", identity.trim_end());
        }
        return format!("{} <{email}> {suffix}", name);
    }
    format!("{} {suffix}", identity.trim_end())
}

/// Normalize author/committer input for writing a commit object.
///
/// Accepts either a full Git header line (`Name <email> <unix> <±HHMM>`) or a contact
/// form (`Name <email>` / bare email). Contact forms are expanded with `now` and an
/// optional `date_override` suffix (same rules as [`author_identity_line`]).
///
/// # Errors
///
/// Returns [`CommitIdentityError`] when the string is empty, contains embedded controls,
/// or cannot be interpreted as a valid identity.
pub fn prepare_commit_identity_line(
    raw: &str,
    date_override: Option<&str>,
    now: OffsetDateTime,
) -> Result<String, CommitIdentityError> {
    if identity_contains_embedded_control(raw) {
        return Err(CommitIdentityError::EmbeddedControl);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CommitIdentityError::Author(AuthorArgError::InvalidFormat));
    }
    if matches!(parse_signature_tail(trimmed), Some(SignatureTail::Valid(_))) {
        let line = trimmed.to_owned();
        return Ok(match date_override {
            Some(suffix) => replace_identity_date_suffix(&line, suffix),
            None => line,
        });
    }
    if let Some(suffix) = date_override {
        if parse_identity_name_email(trimmed).is_some() {
            return Ok(replace_identity_date_suffix(trimmed, suffix));
        }
    }
    let contact = parse_author_arg(trimmed)?;
    Ok(author_identity_line(&contact, date_override, now))
}

/// Like [`prepare_commit_identity_line`], but preserves already-valid header bytes from an
/// existing commit when `author_date` is not overridden.
pub(crate) fn prepare_commit_author_from_parent(
    parent_author: &str,
    parent_author_raw: &[u8],
    author_date: Option<&str>,
    now: OffsetDateTime,
) -> Result<(String, Vec<u8>), CommitIdentityError> {
    if let Some(suffix) = author_date {
        if identity_contains_embedded_control(parent_author) {
            return Err(CommitIdentityError::EmbeddedControl);
        }
        let trimmed = parent_author.trim();
        if trimmed.is_empty() {
            return Err(CommitIdentityError::Author(AuthorArgError::InvalidFormat));
        }
        return Ok((replace_identity_date_suffix(trimmed, suffix), Vec::new()));
    }
    if !parent_author_raw.is_empty() {
        return Ok((parent_author.to_owned(), parent_author_raw.to_vec()));
    }
    Ok((
        prepare_commit_identity_line(parent_author, None, now)?,
        Vec::new(),
    ))
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
    fn signature_from_str_round_trips_header_line() {
        let line = "Ada Lovelace <ada@example.com> 1700000000 +0530";
        let sig: Signature = line.parse().expect("parse");
        assert_eq!(sig.name, "Ada Lovelace");
        assert_eq!(sig.header_line(), line);
    }

    #[test]
    fn signature_from_str_rejects_contact_and_injected_lines() {
        for line in [
            "Ada <ada@example.com>",
            "A <a@b> <c@d> 1 +0000",
            "Name <a@b>\nInjected: x 1 +0000",
            "A <e@x> totally_bogus -0700",
        ] {
            assert_eq!(
                line.parse::<Signature>(),
                Err(SignatureError::Malformed),
                "{line:?}"
            );
        }
        assert!(matches!(
            "Bad\u{1} <a@b> 1 +0000".parse::<Signature>(),
            Err(SignatureError::ForbiddenCharacter { .. })
        ));
    }

    #[test]
    fn author_contact_signature_applies_date_override() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch");
        let contact = parse_author_arg("Doc <doc@example.com>").expect("contact");
        assert_eq!(
            contact.signature(None, now).expect("now").header_line(),
            "Doc <doc@example.com> 1700000000 +0000"
        );
        assert_eq!(
            contact
                .signature(Some("1234567890 -0800"), now)
                .expect("override")
                .header_line(),
            "Doc <doc@example.com> 1234567890 -0800"
        );
        assert_eq!(
            contact.signature(Some("not a date"), now),
            Err(SignatureError::Malformed)
        );
    }

    #[test]
    fn parse_signature_malformed_identity_is_none() {
        assert!(parse_signature("A <e@x> totally_bogus -0700").is_none());
        assert!(parse_signature("not an identity").is_none());
    }

    #[test]
    fn prepare_commit_identity_line_expands_contact_form() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch");
        let line =
            prepare_commit_identity_line("Doc <doc@example.com>", None, now).expect("prepare");
        assert!(parse_signature(&line).is_some(), "full line: {line}");
    }

    #[test]
    fn prepare_commit_identity_line_accepts_full_header() {
        let ident = "Ada Lovelace <ada@example.com> 1700000000 +0530";
        let now = OffsetDateTime::from_unix_timestamp(1).expect("epoch");
        let line = prepare_commit_identity_line(ident, None, now).expect("prepare");
        assert_eq!(line, ident);
    }

    #[test]
    fn prepare_commit_identity_line_rejects_embedded_newline() {
        let now = OffsetDateTime::from_unix_timestamp(1).expect("epoch");
        assert_eq!(
            prepare_commit_identity_line("Name <a@b>\nInjected", None, now),
            Err(CommitIdentityError::EmbeddedControl)
        );
    }

    #[test]
    fn prepare_commit_identity_line_rejects_bare_name_without_email() {
        let now = OffsetDateTime::from_unix_timestamp(1).expect("epoch");
        assert!(matches!(
            prepare_commit_identity_line("no email here", None, now),
            Err(CommitIdentityError::Author(AuthorArgError::InvalidFormat))
        ));
    }

    #[test]
    fn prepare_commit_identity_line_date_override_rewrites_non_numeric_tail() {
        let ident = "A U Thor <author@example.com> totally_bogus -0700";
        let now = OffsetDateTime::from_unix_timestamp(1).expect("epoch");
        let line =
            prepare_commit_identity_line(ident, Some("946684800 +0000"), now).expect("override");
        assert_eq!(line, "A U Thor <author@example.com> 946684800 +0000");
        assert!(parse_signature(&line).is_some());
    }

    #[test]
    fn prepare_commit_author_from_parent_date_override_rewrites_non_numeric_tail() {
        let parent = "A U Thor <author@example.com> totally_bogus -0700";
        let now = OffsetDateTime::from_unix_timestamp(1).expect("epoch");
        let (line, raw) = prepare_commit_author_from_parent(
            parent,
            parent.as_bytes(),
            Some("946684800 +0000"),
            now,
        )
        .expect("override");
        assert!(raw.is_empty());
        assert_eq!(line, "A U Thor <author@example.com> 946684800 +0000");
    }
}

//! Commit-metadata helpers shared by the porcelain commands.
//!
//! This module holds pure-domain pieces of commit creation that compute a
//! result from plain inputs and carry **no** presentation, argv, or process
//! state. The first piece extracted is the date normalisation used to fill the
//! author/committer timestamp: turning a user-supplied `--date` string (or
//! `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`) into Git's stored `<epoch>
//! <offset>` form. It is shared by `commit`, `commit --amend`, and the
//! sequencer commands (`rebase`, `cherry-pick`, `revert`, `stash`, `notes`,
//! `tag`, `format-patch`, `checkout`).
//!
//! Commit creation from the index (write-tree, object write, ref/reflog updates)
//! lives in [`crate::porcelain::commit`]; the `grit commit` command stages paths
//! then calls that API.
//!
//! Library guide: [commit](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::git_date::parse::{parse_date, DateParseError};

/// Normalise a date string into Git's stored `<epoch> <offset>` timestamp.
///
/// Accepts the forms Git's `commit`/`--date` path understands: RFC 3339 / ISO
/// 8601 (with or without an explicit zone, in which case UTC is assumed),
/// `YYYY-MM-DD HH:MM:SS <tz>`, `@<epoch> <tz>`, and the looser
/// approxidate-style strings handled by [`parse_date`]. Returns `None` when the
/// input is already in `<epoch> <offset>` form (nothing to convert) or cannot
/// be parsed; callers then fall back to using the raw string.
pub fn parse_date_to_git_timestamp(date_str: &str) -> Option<String> {
    let trimmed = date_str.trim();

    // ISO 8601 / RFC 3339, including forms Git accepts without an explicit offset
    // (e.g. `2020-01-01T00:00:00` — treated as UTC when no zone is present).
    if let Ok(dt) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Some(format_git_timestamp(dt));
    }
    let with_utc_z = format!("{trimmed}Z");
    if let Ok(dt) = OffsetDateTime::parse(&with_utc_z, &Rfc3339) {
        return Some(format_git_timestamp(dt));
    }

    // Already in `<epoch> <offset>` format? (epoch is all digits)
    let parts: Vec<&str> = trimmed.rsplitn(2, ' ').collect();
    if parts.len() == 2 {
        let maybe_epoch = parts[1];
        if maybe_epoch.chars().all(|c| c.is_ascii_digit()) {
            // Already epoch + offset
            return None;
        }
    }

    // Try parsing "YYYY-MM-DD HH:MM:SS <tz>" format
    if parts.len() == 2 {
        let tz = parts[0];
        let datetime = parts[1];

        // Parse tz offset
        let tz_bytes = tz.as_bytes();
        if tz_bytes.len() >= 5 {
            let sign: i64 = if tz_bytes[0] == b'-' { -1 } else { 1 };
            let h: i64 = tz[1..3].parse().unwrap_or(0);
            let m: i64 = tz[3..5].parse().unwrap_or(0);
            let tz_secs = sign * (h * 3600 + m * 60);

            // Try YYYY-MM-DD HH:MM:SS
            if let Ok(offset) = time::UtcOffset::from_whole_seconds(tz_secs as i32) {
                let fmt = time::format_description::parse_borrowed::<1>(
                    "[year]-[month]-[day] [hour]:[minute]:[second]",
                )
                .ok()?;
                if let Ok(naive) = time::PrimitiveDateTime::parse(datetime, &fmt) {
                    let dt = naive.assume_offset(offset);
                    let epoch = dt.unix_timestamp();
                    return Some(format!("{epoch} {tz}"));
                }
            }
        }
    }

    // Try "@<epoch>" format (git uses this for testing)
    if let Some(epoch_str) = trimmed.strip_prefix('@') {
        // @<epoch> <tz>
        let ep_parts: Vec<&str> = epoch_str.splitn(2, ' ').collect();
        if ep_parts.len() == 2 {
            if let Ok(_epoch) = ep_parts[0].parse::<i64>() {
                return Some(format!("{} {}", ep_parts[0], ep_parts[1]));
            }
        }
    }

    // Loose Git dates without explicit zone (e.g. `2022-02-01 00:00` from GIT_COMMITTER_DATE).
    if let Ok(canonical) = parse_date(trimmed) {
        return Some(canonical);
    }

    None
}

/// True when `s` is already Git's canonical `<unix-seconds> <±HHMM>` form with a valid zone.
fn is_canonical_epoch_numeric_tz(s: &str) -> bool {
    let Some((epoch, tz)) = s.rsplit_once(' ') else {
        return false;
    };
    if epoch.is_empty() || !epoch.as_bytes().iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let tz = tz.as_bytes();
    if tz.len() != 5 || (tz[0] != b'+' && tz[0] != b'-') {
        return false;
    }
    if !tz[1..].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let hh = (tz[1] - b'0') as u32 * 10 + (tz[2] - b'0') as u32;
    let mm = (tz[3] - b'0') as u32 * 10 + (tz[4] - b'0') as u32;
    hh <= 23 && mm <= 59
}

/// Error when a `--trailer` argument is not `key=value` or `key: value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailerArgParseError {
    raw: String,
}

impl std::fmt::Display for TrailerArgParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "trailer must be key=value or key: value (got '{}')",
            self.raw
        )
    }
}

impl std::error::Error for TrailerArgParseError {}

/// Split a `git commit --trailer` argument into token and value.
///
/// The separator is the first `:` or `=` in the string (whichever comes first),
/// matching Git's `parse_trailer_arg` / `git interpret-trailers --trailer`.
///
/// # Errors
///
/// Returns [`TrailerArgParseError`] when neither separator appears.
pub fn parse_trailer_command_line_arg(raw: &str) -> Result<(String, String), TrailerArgParseError> {
    let raw = raw.trim();
    let sep_pos = raw
        .char_indices()
        .find(|(_, c)| *c == ':' || *c == '=')
        .map(|(i, _)| i);
    let Some(pos) = sep_pos else {
        return Err(TrailerArgParseError {
            raw: raw.to_owned(),
        });
    };
    Ok((
        raw[..pos].trim().to_owned(),
        raw[pos + 1..].trim().to_owned(),
    ))
}

/// Normalise a user-supplied author/committer date (CLI `--date`, env overrides) into
/// Git's stored `<epoch> <±HHMM>` form.
///
/// Canonical raw timestamps pass through unchanged. Other strings are parsed with the
/// same rules as Git's `parse_date` (invalid zone tokens such as `garbage` become
/// `+0000`, matching `git commit --date`).
///
/// # Errors
///
/// Returns [`DateParseError`] when the string cannot be interpreted as any date Git accepts.
pub fn normalize_commit_date_override(raw: &str) -> Result<String, DateParseError> {
    let trimmed = raw.trim();
    if is_canonical_epoch_numeric_tz(trimmed) {
        return Ok(trimmed.to_owned());
    }
    if let Some(ts) = parse_date_to_git_timestamp(trimmed) {
        if is_canonical_epoch_numeric_tz(&ts) {
            return Ok(ts);
        }
    }
    parse_date(trimmed)
}

/// Format a timestamp in Git's format: `<epoch> <offset>`.
pub fn format_git_timestamp(dt: OffsetDateTime) -> String {
    let epoch = dt.unix_timestamp();
    let offset = dt.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    format!("{epoch} {hours:+03}{minutes:02}")
}

/// Assemble a commit identity line (`Name <email> <epoch> <offset>`) from an
/// already-resolved name and email plus a timestamp.
///
/// `date_override` is the raw value of a `GIT_AUTHOR_DATE` /
/// `GIT_COMMITTER_DATE`-style environment variable, if set: it is parsed with
/// [`parse_date_to_git_timestamp`] and used verbatim if parsing fails (matching
/// Git's lenient behavior). When `None`, `now` is formatted instead.
#[must_use]
pub fn assemble_identity(
    name: &str,
    email: &str,
    date_override: Option<&str>,
    now: OffsetDateTime,
) -> String {
    let timestamp = match date_override {
        Some(d) => {
            let trimmed = d.trim();
            if let Some(rest) = trimmed.strip_prefix('@') {
                let mut parts = rest.split_whitespace();
                if let Some(epoch_str) = parts.next() {
                    if parts.next().is_none() {
                        if let Ok(epoch) = epoch_str.parse::<i64>() {
                            if let Ok(dt) = OffsetDateTime::from_unix_timestamp(epoch)
                                .map(|t| t.to_offset(now.offset()))
                            {
                                return format!("{name} <{email}> {}", format_git_timestamp(dt));
                            }
                        }
                    }
                }
            }
            normalize_commit_date_override(d).unwrap_or_else(|_| d.to_string())
        }
        None => format_git_timestamp(now),
    };
    format!("{name} <{email}> {timestamp}")
}

/// Commit encoding helpers for non-UTF-8 commit messages.
pub mod encoding {
    pub use crate::commit_encoding::*;
}

/// Commit trailers and interpretation helpers.
pub mod trailers {
    pub use crate::commit_trailers::*;
    /// Parse and apply trailer commands.
    pub mod interpret {
        pub use crate::interpret_trailers::*;
    }
}

/// OpenPGP / SSH signature verification.
pub mod signing {
    pub use crate::signing::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::Environment;
    use std::ffi::OsString;
    use time::OffsetDateTime;

    #[test]
    fn assemble_identity_formats_supplied_now_without_override() {
        let now = OffsetDateTime::from_unix_timestamp(1_112_911_993).unwrap();
        let line = assemble_identity("A U Thor", "author@example.com", None, now);
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 +0000");
    }

    #[test]
    fn assemble_identity_respects_tz_from_environment() {
        let vars = [("TZ", "+0200")]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = Environment::from_vars(vars, std::path::PathBuf::from("/tmp"));
        let now = crate::environment::offset_from_epoch_and_tz(1_112_911_993, env.tz.as_deref());
        let line = assemble_identity("A U Thor", "author@example.com", None, now);
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 +0200");
    }

    #[test]
    fn assemble_identity_parses_date_override() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let line = assemble_identity(
            "A U Thor",
            "author@example.com",
            Some("2005-04-07T22:13:13 +0000"),
            now,
        );
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 +0000");
    }

    #[test]
    fn parse_trailer_command_line_arg_splits_on_first_colon_or_equals() {
        let (k, v) = parse_trailer_command_line_arg("Link: https://x?a=b").unwrap();
        assert_eq!(k, "Link");
        assert_eq!(v, "https://x?a=b");
        let (k, v) = parse_trailer_command_line_arg("Acked-by: Alice=Bob").unwrap();
        assert_eq!(k, "Acked-by");
        assert_eq!(v, "Alice=Bob");
        let (k, v) = parse_trailer_command_line_arg("key=value").unwrap();
        assert_eq!(k, "key");
        assert_eq!(v, "value");
    }

    #[test]
    fn normalize_commit_date_override_rejects_invalid_tz_suffix() {
        assert_eq!(
            normalize_commit_date_override("946684800 garbage").unwrap(),
            "946684800 +0000"
        );
        assert_eq!(
            normalize_commit_date_override("@946684800 garbage").unwrap(),
            "946684800 +0000"
        );
        assert_eq!(
            normalize_commit_date_override("946684800 +0000").unwrap(),
            "946684800 +0000"
        );
        assert_eq!(
            normalize_commit_date_override("946684800 +0530").unwrap(),
            "946684800 +0530"
        );
    }

    #[test]
    fn assemble_identity_normalizes_bad_tz_suffix_like_git() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let line = assemble_identity(
            "A U Thor",
            "author@example.com",
            Some("946684800 garbage"),
            now,
        );
        assert_eq!(line, "A U Thor <author@example.com> 946684800 +0000");
    }

    #[test]
    fn assemble_identity_at_epoch_uses_now_offset() {
        let vars = [("TZ", "America/New_York")]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = Environment::from_vars(vars, std::path::PathBuf::from("/tmp"));
        let now = crate::environment::offset_from_epoch_and_tz(1_112_911_993, env.tz.as_deref());
        let line = assemble_identity("A U Thor", "author@example.com", Some("@1112911993"), now);
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 -0400");
    }
}

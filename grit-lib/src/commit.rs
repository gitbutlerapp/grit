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

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::environment::offset_from_epoch_and_tz;
use crate::git_date::parse::parse_date;

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

/// [`OffsetDateTime`] for a repository wall clock and optional `TZ`-style zone string.
#[must_use]
pub fn identity_now_from_epoch(epoch: i64, tz: Option<&str>) -> OffsetDateTime {
    offset_from_epoch_and_tz(epoch, tz)
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
            parse_date_to_git_timestamp(d).unwrap_or_else(|| d.to_string())
        }
        None => format_git_timestamp(now),
    };
    format!("{name} <{email}> {timestamp}")
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

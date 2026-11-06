//! Human-oriented commit one-line formats shared by porcelain commands.

use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::odb::Odb;

/// Abbreviate `oid` to at most `abbrev_len` hex characters (minimum 4, maximum 40).
#[must_use]
pub fn abbrev_hex(oid: &ObjectId, abbrev_len: usize) -> String {
    let hex = oid.to_hex();
    let n = abbrev_len.clamp(4, 40).min(hex.len());
    hex[..n].to_owned()
}

/// Git's automatic minimum abbreviation length from approximate object count.
///
/// Matches Git's `odb_find_abbrev_len` when `min_length < 0` (MSB of count, halved for hex digits,
/// floored at 7).
#[must_use]
pub fn auto_abbrev_len_for_object_count(count: usize) -> usize {
    const FALLBACK: usize = 7;
    if count == 0 {
        return FALLBACK;
    }
    let mut val = count as u64;
    let mut msb = 0u32;
    while val > 1 {
        val >>= 1;
        msb += 1;
    }
    let bit_width = msb + 1;
    let hex_min = (bit_width as usize).div_ceil(2);
    hex_min.max(FALLBACK)
}

/// Approximate object count for auto `core.abbrev` (local pack index sizes; loose objects are
/// not counted, as in Git).
///
/// # Errors
///
/// Propagates ODB pack listing and loose enumeration errors.
pub fn approximate_object_count(odb: &Odb) -> Result<usize> {
    odb.approximate_object_count()
}

/// Minimum abbreviated oid length from `core.abbrev` and object count (Git-compatible).
///
/// Missing `core.abbrev` or the value `auto` uses Git-compatible automatic length from
/// the approximate object count (same rule as `auto_abbrev_len_for_object_count`).
/// Numeric values are clamped to 4–40.
///
/// # Errors
///
/// Propagates ODB object-count errors.
pub fn resolve_core_abbrev_len(config: &ConfigSet, odb: &Odb) -> Result<usize> {
    if let Some(v) = config.value("core.abbrev") {
        if v.eq_ignore_ascii_case("auto") {
            let count = approximate_object_count(odb)?;
            return Ok(auto_abbrev_len_for_object_count(count));
        }
        if let Ok(n) = v.parse::<i64>() {
            return Ok((n as usize).clamp(4, 40));
        }
    }
    let count = approximate_object_count(odb)?;
    Ok(auto_abbrev_len_for_object_count(count))
}

/// Return the pretty subject for a commit or tag message.
#[must_use]
pub fn message_subject(message: &str) -> String {
    let mut subject_lines = Vec::new();
    for line in MessageLines::new(message) {
        if line.is_empty() {
            if !subject_lines.is_empty() {
                break;
            }
            continue;
        }
        subject_lines.push(line);
    }
    subject_lines.join(" ")
}

struct MessageLines<'a> {
    message: &'a str,
    pos: usize,
}

impl<'a> MessageLines<'a> {
    fn new(message: &'a str) -> Self {
        Self { message, pos: 0 }
    }
}

impl<'a> Iterator for MessageLines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.message.len() {
            return None;
        }
        let start = self.pos;
        let tail = &self.message[start..];
        let newline_rel = tail.find('\n');
        let (mut end, next_start) = match newline_rel {
            Some(rel) => (start + rel, start + rel + 1),
            None => (self.message.len(), self.message.len()),
        };
        if self.message.as_bytes().get(end.wrapping_sub(1)) == Some(&b'\r') && end > start {
            end -= 1;
        }
        self.pos = next_start;
        Some(&self.message[start..end])
    }
}

fn parse_tz_offset_seconds(offset: &str) -> i64 {
    if offset.len() < 5 {
        return 0;
    }
    let sign = if offset.starts_with('-') { -1i64 } else { 1i64 };
    let hours: i64 = offset[1..3].parse().unwrap_or(0);
    let minutes: i64 = offset[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

/// Format the author/committer date as `YYYY-MM-DD` in the commit's local timezone.
#[must_use]
pub fn format_short_date_from_ident(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() < 2 {
        return ident.to_owned();
    }
    let ts_str = parts[1];
    let offset_str = parts[0];
    let Ok(ts) = ts_str.parse::<i64>() else {
        return ident.to_owned();
    };
    let offset_secs = parse_tz_offset_seconds(offset_str);
    let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(ts + offset_secs) else {
        return ident.to_owned();
    };
    let format = time::format_description::parse_borrowed::<1>("[year]-[month]-[day]");
    let Ok(fmt) = format else {
        return ident.to_owned();
    };
    dt.format(&fmt).unwrap_or_else(|_| ident.to_owned())
}

/// One-line `reference` format: `abbrev (subject, YYYY-MM-DD)`.
#[must_use]
pub fn format_reference_line(
    oid: &ObjectId,
    subject_first_line: &str,
    committer_ident: &str,
    abbrev_len: usize,
) -> String {
    let abbrev = abbrev_hex(oid, abbrev_len);
    let date = format_short_date_from_ident(committer_ident);
    format!("{abbrev} ({subject_first_line}, {date})")
}

#[cfg(test)]
mod resolve_core_abbrev_tests {
    use crate::repo::{InitOptions, Repository};

    use super::{auto_abbrev_len_for_object_count, resolve_core_abbrev_len};

    #[test]
    fn auto_abbrev_len_floors_at_seven() {
        assert_eq!(auto_abbrev_len_for_object_count(0), 7);
        assert_eq!(auto_abbrev_len_for_object_count(1), 7);
    }

    #[test]
    fn resolve_core_abbrev_default_matches_auto_for_empty_repo() -> crate::error::Result<()> {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let config = (*repo.config().snapshot()?).clone();
        let len = resolve_core_abbrev_len(&config, repo.odb())?;
        assert_eq!(len, auto_abbrev_len_for_object_count(0));
        Ok(())
    }

    #[test]
    fn approximate_object_count_ignores_loose_objects() -> crate::error::Result<()> {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        for n in 0..3 {
            repo.odb()
                .write(crate::objects::ObjectKind::Blob, format!("{n}").as_bytes())?;
        }
        assert_eq!(repo.odb().count_loose_objects()?, 3);
        assert_eq!(super::approximate_object_count(repo.odb())?, 0);
        Ok(())
    }
}

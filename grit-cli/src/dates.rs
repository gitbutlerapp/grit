//! Date formatting for JSON and human CLI output.

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// Parse `"<epoch> <tz>"` from a Git identity line suffix into RFC 3339.
#[must_use]
pub fn rfc3339_from_identity_when(when: &str) -> String {
    let mut parts = when.split_whitespace();
    let Some(epoch) = parts.next().and_then(|s| s.parse::<i64>().ok()) else {
        return when.trim().to_owned();
    };
    let tz = parts.next().unwrap_or("+0000");
    rfc3339_from_epoch_and_tz(epoch, tz).unwrap_or_else(|| when.trim().to_owned())
}

/// Format a Unix epoch with a Git `±HHMM` zone as RFC 3339.
pub fn rfc3339_from_epoch_and_tz(epoch: i64, tz: &str) -> Option<String> {
    let offset = parse_git_tz_offset(tz)?;
    let dt = OffsetDateTime::from_unix_timestamp(epoch).ok()?;
    let local = dt.to_offset(offset);
    local.format(&Rfc3339).ok()
}

/// Extract the `"epoch tz"` suffix from a `Name <email> epoch tz` identity line.
#[must_use]
pub fn identity_when_suffix(ident: &str) -> &str {
    ident
        .rsplit_once('>')
        .map(|(_, after)| after.trim())
        .unwrap_or("")
}

fn parse_git_tz_offset(tz: &str) -> Option<UtcOffset> {
    if tz.len() < 5 {
        return Some(UtcOffset::UTC);
    }
    let sign: i32 = if tz.starts_with('-') { -1 } else { 1 };
    let hours: i32 = tz[1..3].parse().ok()?;
    let minutes: i32 = tz[3..5].parse().ok()?;
    let total_secs = sign * (hours * 3600 + minutes * 60);
    UtcOffset::from_whole_seconds(total_secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_uses_recorded_offset() {
        let s = rfc3339_from_identity_when("1700000000 +0000");
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn rfc3339_negative_subhour_offset() {
        let s = rfc3339_from_identity_when("1700000000 -0030");
        assert_eq!(s, "2023-11-14T21:43:20-00:30");
    }
}

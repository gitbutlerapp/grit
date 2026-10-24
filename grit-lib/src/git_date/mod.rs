//! Git-compatible date parsing and display.

pub mod approx;
pub(crate) mod compat;
pub mod parse;
pub mod show;
pub mod tm;

use thiserror::Error;

/// Failure to interpret a Git-style date cutoff string (for example `--since` / `--until`).
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DateSpecError {
    /// The specification could not be parsed as a date.
    #[error("unparseable date spec: {spec:?}")]
    Unparseable {
        /// Original input string.
        spec: String,
    },
}

/// Parse a Git-style date cutoff using an injected reference time.
///
/// Relative forms such as `3.days.ago`, `yesterday`, and `noon` are resolved against
/// `now_sec` in the **process timezone** (same as Git `approxidate`). Absolute forms
/// include `@<unix-seconds>`, ISO 8601, and RFC 2822.
///
/// Unlike Git's silent fallback to "now" on some invalid input, this returns
/// [`DateSpecError::Unparseable`] when the approxidate parser sets its error flag.
///
/// # Parameters
///
/// - `spec` — Git approxidate string (for example `2.weeks.ago`, `yesterday`, `@1700000000`).
/// - `now_sec` — Unix seconds used as “now” when resolving relative fields.
///
/// # Returns
///
/// Unix timestamp in seconds for the cutoff instant (commits strictly after this time
/// match a `--since` filter).
///
/// # Errors
///
/// Returns [`DateSpecError::Unparseable`] when the spec cannot be parsed or the
/// resulting timestamp does not fit in [`i64`].
///
/// # Examples
///
/// ```
/// use grit_lib::config::git_date::parse_date_cutoff;
///
/// let now = 1_700_100_000_i64;
/// let cutoff = parse_date_cutoff("1.day.ago", now).expect("relative cutoff");
/// assert_eq!(cutoff, now - 86_400);
/// ```
pub fn parse_date_cutoff(spec: &str, now_sec: i64) -> Result<i64, DateSpecError> {
    let trimmed = spec.trim();
    let mut err = 0;
    let ts = approx::approxidate_careful_at(trimmed, now_sec, Some(&mut err));
    if err != 0 {
        return Err(DateSpecError::Unparseable {
            spec: spec.to_string(),
        });
    }
    i64::try_from(ts).map_err(|_| DateSpecError::Unparseable {
        spec: spec.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_date_cutoff;
    use super::DateSpecError;

    const NOW: i64 = 1_700_100_000;

    #[test]
    fn cutoff_rejects_garbage() {
        assert_eq!(
            parse_date_cutoff("", NOW),
            Err(DateSpecError::Unparseable {
                spec: String::new()
            })
        );
        assert!(matches!(
            parse_date_cutoff("!!!not-a-date!!!", NOW),
            Err(DateSpecError::Unparseable { .. })
        ));
    }
}

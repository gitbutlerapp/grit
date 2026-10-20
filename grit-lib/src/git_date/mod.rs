//! Git-compatible date parsing and display.

pub mod approx;
pub(crate) mod compat;
pub mod parse;
pub mod show;
pub mod tm;

use show::{parse_date_format, DateMode, DateModeType};
use std::mem::size_of;
use thiserror::Error;
use tm::{atoi_bytes, parse_timestamp_prefix, Timestamp};

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
/// `now_sec`. Absolute forms include `@<unix-seconds>`, ISO 8601, and RFC 2822.
///
/// Unlike Git's silent fallback to "now" on some invalid input, this returns
/// [`DateSpecError::Unparseable`] when the approxidate parser sets its error flag.
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

    fn git_max_age(spec: &str, now: i64) -> Option<i64> {
        use std::process::Command;
        let out = Command::new("git")
            .env("GIT_TEST_DATE_NOW", now.to_string())
            .env("TZ", "UTC")
            .args(["rev-parse", &format!("--since={spec}")])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout);
        s.trim().strip_prefix("--max-age=")?.parse().ok()
    }

    #[test]
    fn cutoff_relative_units() {
        for spec in [
            "1.second.ago",
            "2.minutes.ago",
            "3.hours.ago",
            "1.day.ago",
            "2.weeks.ago",
            "1.month.ago",
            "1.year.ago",
        ] {
            let got = parse_date_cutoff(spec, NOW).expect(spec);
            let git = git_max_age(spec, NOW).unwrap_or_else(|| panic!("git {spec}"));
            assert_eq!(got, git, "spec {spec:?}");
        }
    }

    #[test]
    fn cutoff_iso_and_rfc2822() {
        for spec in [
            "2024-01-15T12:00:00Z",
            "2024-01-15 12:00:00 +0000",
            "Thu, 15 Jan 2024 12:00:00 +0000",
        ] {
            let got = parse_date_cutoff(spec, NOW).expect(spec);
            let git = git_max_age(spec, NOW).unwrap_or_else(|| panic!("git {spec}"));
            assert_eq!(got, git, "spec {spec:?}");
        }
        let got = parse_date_cutoff("yesterday", NOW).expect("yesterday");
        let git = git_max_age("yesterday", NOW).expect("git yesterday");
        assert_eq!(got, git);
        let got = parse_date_cutoff("noon", NOW).expect("noon");
        let git = git_max_age("noon", NOW).expect("git noon");
        assert_eq!(got, git);
    }

    #[test]
    fn cutoff_epoch_at_form() {
        let spec = "@1700000000";
        let got = parse_date_cutoff(spec, NOW).expect("@epoch");
        assert_eq!(got, 1_700_000_000);
        let git = git_max_age(spec, NOW).expect("git @epoch");
        assert_eq!(got, git);
    }

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

/// Result of `test-tool date` — either lines for stdout or a process exit code (no output).
pub enum TestToolDateResult {
    /// `Output(Vec<String>)` variant.
    Output(Vec<String>),
    /// `Exit(i32)` variant.
    Exit(i32),
}

/// Run `test-tool date` (see `git/t/helper/test-date.c`).
pub fn test_tool_date(
    env: &crate::environment::Environment,
    args: &[String],
) -> Result<TestToolDateResult, String> {
    let mut env = env.clone();
    // Match Git's `test-lib.sh` (`TZ=UTC`) when harness sets `GIT_TEST_DATE_NOW` but leaves `TZ`
    // unset (direct `sh t0006-date.sh` runs). Do not override an explicit `TZ` (e.g. `EST5`).
    if env.var("GIT_TEST_DATE_NOW").is_some() && env.tz.is_none() {
        env.tz = Some("UTC".into());
    }
    if args.is_empty() {
        return Err("test-tool date: missing subcommand".to_string());
    }
    let sub = args[0].as_str();
    let rest = &args[1..];

    match sub {
        "is64bit" => {
            // Match Git's `test-tool date is64bit` (`test-date.c`): `sizeof(timestamp_t) == 8`.
            let code = if size_of::<Timestamp>() == 8 { 0 } else { 1 };
            Ok(TestToolDateResult::Exit(code))
        }
        "time_t-is64bit" => {
            let code = if size_of::<compat::time_t>() == 8 {
                0
            } else {
                1
            };
            Ok(TestToolDateResult::Exit(code))
        }
        "relative" => {
            let mut lines = Vec::new();
            for a in rest {
                let t: u64 = a
                    .parse()
                    .map_err(|_| format!("test-tool date relative: bad integer {a}"))?;
                let s = show::show_date_relative(t, tm::process_wall_clock_sec());
                lines.push(format!("{a} -> {s}"));
            }
            Ok(TestToolDateResult::Output(lines))
        }
        "human" => {
            let mut lines = Vec::new();
            for a in rest {
                let t: u64 = a
                    .parse()
                    .map_err(|_| format!("test-tool date human: bad integer {a}"))?;
                let mut mode = DateMode::from_type(DateModeType::Human);
                let s = show::show_date(t, 0, &mut mode);
                show::date_mode_release(&mut mode);
                lines.push(format!("{a} -> {s}"));
            }
            Ok(TestToolDateResult::Output(lines))
        }
        "parse" => {
            let mut lines = Vec::new();
            for a in rest {
                match parse::parse_date(a) {
                    Ok(ds) => {
                        let parts: Vec<&str> = ds.split_whitespace().collect();
                        if parts.len() >= 2 {
                            let t: u64 = parts[0].parse().map_err(|_| "bad parse output")?;
                            let tz = atoi_bytes(parts[1].as_bytes());
                            let mut mode = DateMode::from_type(DateModeType::Iso8601);
                            let out = show::show_date(t, tz, &mut mode);
                            show::date_mode_release(&mut mode);
                            lines.push(format!("{a} -> {out}"));
                        } else {
                            lines.push(format!("{a} -> bad"));
                        }
                    }
                    Err(parse::DateParseError) => lines.push(format!("{a} -> bad")),
                }
            }
            Ok(TestToolDateResult::Output(lines))
        }
        "approxidate" => {
            let mut lines = Vec::new();
            for a in rest {
                let mut err = 0;
                let t = approx::approxidate_careful(a, Some(&mut err));
                let mut mode = DateMode::from_type(DateModeType::Iso8601);
                let out = show::show_date(t, 0, &mut mode);
                show::date_mode_release(&mut mode);
                lines.push(format!("{a} -> {out}"));
            }
            Ok(TestToolDateResult::Output(lines))
        }
        "timestamp" => {
            let mut lines = Vec::new();
            for a in rest {
                let mut err = 0;
                let t = approx::approxidate_careful(a, Some(&mut err));
                lines.push(if err == 0 {
                    format!("{a} -> {t}")
                } else {
                    format!("{a} -> bad")
                });
            }
            Ok(TestToolDateResult::Output(lines))
        }
        s if s.starts_with("show:") => {
            let format = s.strip_prefix("show:").unwrap_or("");
            let mut mode = parse_date_format(format).map_err(|e| e.to_string())?;
            let mut lines = Vec::new();
            for a in rest {
                let b = a.as_bytes();
                let (t, n) = parse_timestamp_prefix(b);
                let mut rest_b = &b[n..];
                while rest_b.first() == Some(&b' ') {
                    rest_b = &rest_b[1..];
                }
                let tz = atoi_bytes(rest_b);
                let s = show::show_date(t, tz, &mut mode);
                lines.push(format!("{a} -> {s}"));
            }
            show::date_mode_release(&mut mode);
            Ok(TestToolDateResult::Output(lines))
        }
        _ => Err(format!("test-tool date: unknown subcommand '{sub}'")),
    }
}

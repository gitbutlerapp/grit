//! Git-compatible date parsing and display.

pub mod approx;
pub(crate) mod compat;
pub mod parse;
pub mod show;
pub mod tm;

use show::{parse_date_format, DateMode, DateModeType};
use std::mem::size_of;
use tm::{atoi_bytes, parse_timestamp_prefix, Timestamp};

/// Result of `test-tool date` — either lines for stdout or a process exit code (no output).
pub enum TestToolDateResult {
    Output(Vec<String>),
    Exit(i32),
}

/// Run `test-tool date` (see `git/t/helper/test-date.c`).
pub fn test_tool_date(args: &[String]) -> Result<TestToolDateResult, String> {
    // Match Git's `test-lib.sh` (`TZ=UTC`) when harness sets `GIT_TEST_DATE_NOW` but leaves `TZ`
    // unset (direct `sh t0006-date.sh` runs). Do not override an explicit `TZ` (e.g. `EST5`).
    if std::env::var_os("GIT_TEST_DATE_NOW").is_some() && std::env::var_os("TZ").is_none() {
        std::env::set_var("TZ", "UTC");
        // POSIX: refresh libc timezone cache after changing TZ (not in all `libc` bindings).
        unsafe extern "C" {
            fn tzset();
        }
        unsafe {
            tzset();
        }
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
                let s = show::show_date_relative(t, tm::get_time_sec());
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

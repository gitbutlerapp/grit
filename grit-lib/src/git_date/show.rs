//! Git-compatible date display (`show_date`, `show_date_relative`, strftime handling).

use super::compat::{self, time_t, tm};
use super::tm::{
    empty_tm, init_tm_unknown, local_time_tzoffset, local_tzoffset, system_now_sec, time_to_tm,
    time_to_tm_local, tm_to_time_t, TzHhmm,
};
use std::ffi::CString;
use std::io::IsTerminal;

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const WEEKDAY_NAMES: [&str; 7] = [
    "Sundays",
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DateModeType {
    Normal,
    Human,
    Relative,
    Short,
    Iso8601,
    Iso8601Strict,
    Rfc2822,
    Strftime,
    Raw,
    Unix,
}

pub struct DateMode {
    pub ty: DateModeType,
    pub local: bool,
    pub strftime_fmt: Option<String>,
}

impl DateMode {
    pub fn from_type(ty: DateModeType) -> Self {
        Self {
            ty,
            local: false,
            strftime_fmt: None,
        }
    }
}

pub fn parse_date_format(format: &str) -> Result<DateMode, &'static str> {
    let mut s = format;
    if let Some(rest) = s.strip_prefix("auto:") {
        s = if std::io::stdout().is_terminal() {
            rest
        } else {
            "default"
        };
    }
    if s == "local" {
        s = "default-local";
    }
    let (ty, mut p) = parse_date_type(s)?;
    let mut local = false;
    if let Some(r) = p.strip_prefix("-local") {
        local = true;
        p = r;
    }
    let mut mode = DateMode {
        ty,
        local,
        strftime_fmt: None,
    };
    if ty == DateModeType::Strftime {
        let rest = p
            .strip_prefix(':')
            .ok_or("date format missing colon separator")?;
        mode.strftime_fmt = Some(rest.to_string());
    } else if !p.is_empty() {
        return Err("unknown date format");
    }
    Ok(mode)
}

fn parse_date_type(s: &str) -> Result<(DateModeType, &str), &'static str> {
    if let Some(r) = s.strip_prefix("relative") {
        return Ok((DateModeType::Relative, r));
    }
    if let Some(r) = s
        .strip_prefix("iso8601-strict")
        .or_else(|| s.strip_prefix("iso-strict"))
    {
        return Ok((DateModeType::Iso8601Strict, r));
    }
    if let Some(r) = s.strip_prefix("iso8601").or_else(|| s.strip_prefix("iso")) {
        return Ok((DateModeType::Iso8601, r));
    }
    if let Some(r) = s.strip_prefix("rfc2822").or_else(|| s.strip_prefix("rfc")) {
        return Ok((DateModeType::Rfc2822, r));
    }
    if let Some(r) = s.strip_prefix("short") {
        return Ok((DateModeType::Short, r));
    }
    if let Some(r) = s.strip_prefix("default") {
        return Ok((DateModeType::Normal, r));
    }
    if let Some(r) = s.strip_prefix("human") {
        return Ok((DateModeType::Human, r));
    }
    if let Some(r) = s.strip_prefix("raw") {
        return Ok((DateModeType::Raw, r));
    }
    if let Some(r) = s.strip_prefix("unix") {
        return Ok((DateModeType::Unix, r));
    }
    if let Some(r) = s.strip_prefix("format") {
        return Ok((DateModeType::Strftime, r));
    }
    Err("unknown date format")
}

pub fn date_mode_release(mode: &mut DateMode) {
    mode.strftime_fmt = None;
}

pub fn show_date_relative(time: u64, now_sec: i64) -> String {
    let now = now_sec as i128;
    let t = time as i128;
    if now < t {
        return "in the future".to_string();
    }
    let mut diff = (now - t) as u64;
    if diff < 90 {
        return if diff == 1 {
            "1 second ago".to_string()
        } else {
            format!("{diff} seconds ago")
        };
    }
    diff = (diff + 30) / 60;
    if diff < 90 {
        return if diff == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{diff} minutes ago")
        };
    }
    diff = (diff + 30) / 60;
    if diff < 36 {
        return if diff == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{diff} hours ago")
        };
    }
    diff = (diff + 12) / 24;
    if diff < 14 {
        return if diff == 1 {
            "1 day ago".to_string()
        } else {
            format!("{diff} days ago")
        };
    }
    if diff < 70 {
        let w = (diff + 3) / 7;
        return if w == 1 {
            "1 week ago".to_string()
        } else {
            format!("{w} weeks ago")
        };
    }
    if diff < 365 {
        let m = (diff + 15) / 30;
        return if m == 1 {
            "1 month ago".to_string()
        } else {
            format!("{m} months ago")
        };
    }
    if diff < 1825 {
        let totalmonths = (diff * 12 * 2 + 365) / (365 * 2);
        let years = totalmonths / 12;
        let months = totalmonths % 12;
        if months > 0 {
            let ys = if years == 1 {
                "1 year".to_string()
            } else {
                format!("{years} years")
            };
            return if months == 1 {
                format!("{ys}, 1 month ago")
            } else {
                format!("{ys}, {months} months ago")
            };
        }
        return if years == 1 {
            "1 year ago".to_string()
        } else {
            format!("{years} years ago")
        };
    }
    let y = (diff + 183) / 365;
    if y == 1 {
        "1 year ago".to_string()
    } else {
        format!("{y} years ago")
    }
}

fn strbuf_rtrim(s: &mut String) {
    while let Some(c) = s.pop() {
        if !c.is_whitespace() {
            s.push(c);
            break;
        }
    }
}

fn show_date_normal(
    time: u64,
    tm: &tm,
    tz: TzHhmm,
    human_tm: &tm,
    human_tz: TzHhmm,
    local: bool,
) -> String {
    #[derive(Clone, Copy)]
    struct Hide {
        year: bool,
        date: bool,
        wday: bool,
        time: bool,
        seconds: bool,
        tz: bool,
    }
    let mut hide = Hide {
        year: false,
        date: false,
        wday: false,
        time: false,
        seconds: false,
        tz: false,
    };

    hide.tz = local || tz == human_tz;
    hide.year = tm.tm_year == human_tm.tm_year;
    if hide.year && tm.tm_mon == human_tm.tm_mon {
        if tm.tm_mday > human_tm.tm_mday {
            // future date in same month
        } else if tm.tm_mday == human_tm.tm_mday {
            hide.date = true;
            hide.wday = true;
        } else if tm.tm_mday + 5 > human_tm.tm_mday {
            hide.date = true;
        }
    }

    if hide.wday {
        return show_date_relative(time, system_now_sec());
    }

    if human_tm.tm_year != 0 {
        hide.seconds = true;
        hide.tz |= !hide.date;
        hide.wday = !hide.year;
        hide.time = !hide.year;
    }

    let mut out = String::new();
    if !hide.wday {
        let w = WEEKDAY_NAMES[tm.tm_wday as usize].as_bytes();
        out.push_str(std::str::from_utf8(&w[..3]).unwrap_or("Sun"));
        out.push(' ');
    }
    if !hide.date {
        let m = MONTH_NAMES[tm.tm_mon as usize].as_bytes();
        out.push_str(std::str::from_utf8(&m[..3]).unwrap_or("Jan"));
        out.push(' ');
        out.push_str(&format!("{} ", tm.tm_mday));
    }
    if !hide.time {
        out.push_str(&format!("{:02}:{:02}", tm.tm_hour, tm.tm_min));
        if !hide.seconds {
            out.push_str(&format!(":{:02}", tm.tm_sec));
        }
    } else {
        strbuf_rtrim(&mut out);
    }
    if !hide.year {
        out.push_str(&format!(" {}", tm.tm_year + 1900));
    }
    if !hide.tz {
        out.push_str(&format!(" {:+05}", tz));
    }
    out
}

fn strbuf_expand_step(munged: &mut String, fmt: &mut &str) -> bool {
    let Some(pct) = fmt.find('%') else {
        munged.push_str(fmt);
        *fmt = "";
        return false;
    };
    munged.push_str(&fmt[..pct]);
    *fmt = &fmt[pct + 1..];
    true
}

pub fn strbuf_addftime(tm: &tm, tz_hhmm: TzHhmm, fmt: &str, suppress_tz_name: bool) -> String {
    if fmt.is_empty() {
        return String::new();
    }
    let mut munged = String::new();
    let mut rest = fmt;
    while strbuf_expand_step(&mut munged, &mut rest) {
        if rest.starts_with('%') {
            munged.push_str("%%");
            rest = &rest[1..];
        } else if rest.starts_with('s') {
            let secs = tm_to_time_t(tm) as i64
                - 3600 * (tz_hhmm / 100) as i64
                - 60 * (tz_hhmm % 100) as i64;
            munged.push_str(&format!("{secs}"));
            rest = &rest[1..];
        } else if rest.starts_with('z') {
            munged.push_str(&format!("{:+05}", tz_hhmm));
            rest = &rest[1..];
        } else if !suppress_tz_name && rest.starts_with('Z') && tz_hhmm == 0 {
            munged.push_str("UTC");
            rest = &rest[1..];
        } else if suppress_tz_name && rest.starts_with('Z') {
            rest = &rest[1..];
        } else {
            munged.push('%');
        }
    }
    strftime_c(&munged, tm)
}

fn strftime_c(fmt: &str, tm: &tm) -> String {
    let mut buf = vec![0u8; 4096];
    let cfmt = match CString::new(fmt) {
        Ok(c) => c,
        // Fall back to a fixed format when `fmt` contains an interior NUL.
        Err(_) => match CString::new("%Y") {
            Ok(c) => c,
            Err(_) => return String::new(),
        },
    };
    unsafe {
        let n = compat::strftime(
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
            cfmt.as_ptr(),
            tm,
        );
        if n == 0 && !fmt.is_empty() {
            let mut munged = fmt.to_string();
            munged.push(' ');
            if let Ok(c2) = CString::new(munged.as_str()) {
                let n2 = compat::strftime(
                    buf.as_mut_ptr() as *mut std::ffi::c_char,
                    buf.len(),
                    c2.as_ptr(),
                    tm,
                );
                if n2 > 0 {
                    return String::from_utf8_lossy(&buf[..n2 - 1]).into_owned();
                }
            }
        }
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn show_date(time: u64, mut tz: TzHhmm, mode: &mut DateMode) -> String {
    if mode.ty == DateModeType::Unix {
        return format!("{time}");
    }
    let mut tmbuf = init_tm_unknown();
    let mut human_tm = empty_tm();
    let mut human_tz: TzHhmm = -1;

    if mode.ty == DateModeType::Human {
        let now = system_now_sec();
        unsafe {
            human_tz = local_time_tzoffset(now as time_t, &mut human_tm);
        }
    }

    if mode.local {
        tz = local_tzoffset(time);
    }

    if mode.ty == DateModeType::Raw {
        return format!("{time} {:+05}", tz);
    }

    if mode.ty == DateModeType::Relative {
        return show_date_relative(time, system_now_sec());
    }

    let mut tz = tz;
    let ok = if mode.local {
        unsafe { time_to_tm_local(time, &mut tmbuf).is_some() }
    } else {
        time_to_tm(time, tz, &mut tmbuf)
    };
    if !ok {
        time_to_tm(0, 0, &mut tmbuf);
        tz = 0;
    }

    match mode.ty {
        DateModeType::Short => format!(
            "{:04}-{:02}-{:02}",
            tmbuf.tm_year + 1900,
            tmbuf.tm_mon + 1,
            tmbuf.tm_mday
        ),
        DateModeType::Iso8601 => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {:+05}",
            tmbuf.tm_year + 1900,
            tmbuf.tm_mon + 1,
            tmbuf.tm_mday,
            tmbuf.tm_hour,
            tmbuf.tm_min,
            tmbuf.tm_sec,
            tz
        ),
        DateModeType::Iso8601Strict => {
            let mut s = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                tmbuf.tm_year + 1900,
                tmbuf.tm_mon + 1,
                tmbuf.tm_mday,
                tmbuf.tm_hour,
                tmbuf.tm_min,
                tmbuf.tm_sec
            );
            if tz == 0 {
                s.push('Z');
            } else {
                let sign = if tz >= 0 { '+' } else { '-' };
                let a = tz.abs();
                s.push(sign);
                s.push_str(&format!("{:02}:{:02}", a / 100, a % 100));
            }
            s
        }
        DateModeType::Rfc2822 => format!(
            "{}, {} {} {} {:02}:{:02}:{:02} {:+05}",
            &WEEKDAY_NAMES[tmbuf.tm_wday as usize][..3],
            tmbuf.tm_mday,
            &MONTH_NAMES[tmbuf.tm_mon as usize][..3],
            tmbuf.tm_year + 1900,
            tmbuf.tm_hour,
            tmbuf.tm_min,
            tmbuf.tm_sec,
            tz
        ),
        DateModeType::Strftime => {
            let fmt = mode.strftime_fmt.as_deref().unwrap_or("");
            strbuf_addftime(&tmbuf, tz, fmt, !mode.local)
        }
        _ => show_date_normal(time, &tmbuf, tz, &human_tm, human_tz, mode.local),
    }
}

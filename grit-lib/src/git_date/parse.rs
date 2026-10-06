//! Git-compatible date parsing (`parse_date_basic`, `parse_date`).

/// Date string could not be parsed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DateParseError;

use super::compat::{self, time_t, tm};
use super::tm::{
    empty_tm, get_time_sec, init_tm_unknown, is_date_known, match_string, maybeiso8601, nodate,
    parse_timestamp_prefix, skip_alpha, tm_to_time_t, TIMESTAMP_MAX,
};

struct TzName {
    name: &'static str,
    offset_hours: i32,
    dst: i32,
}

const TIMEZONE_NAMES: &[TzName] = &[
    TzName {
        name: "IDLW",
        offset_hours: -12,
        dst: 0,
    },
    TzName {
        name: "NT",
        offset_hours: -11,
        dst: 0,
    },
    TzName {
        name: "CAT",
        offset_hours: -10,
        dst: 0,
    },
    TzName {
        name: "HST",
        offset_hours: -10,
        dst: 0,
    },
    TzName {
        name: "HDT",
        offset_hours: -10,
        dst: 1,
    },
    TzName {
        name: "YST",
        offset_hours: -9,
        dst: 0,
    },
    TzName {
        name: "YDT",
        offset_hours: -9,
        dst: 1,
    },
    TzName {
        name: "PST",
        offset_hours: -8,
        dst: 0,
    },
    TzName {
        name: "PDT",
        offset_hours: -8,
        dst: 1,
    },
    TzName {
        name: "MST",
        offset_hours: -7,
        dst: 0,
    },
    TzName {
        name: "MDT",
        offset_hours: -7,
        dst: 1,
    },
    TzName {
        name: "CST",
        offset_hours: -6,
        dst: 0,
    },
    TzName {
        name: "CDT",
        offset_hours: -6,
        dst: 1,
    },
    TzName {
        name: "EST",
        offset_hours: -5,
        dst: 0,
    },
    TzName {
        name: "EDT",
        offset_hours: -5,
        dst: 1,
    },
    TzName {
        name: "AST",
        offset_hours: -3,
        dst: 0,
    },
    TzName {
        name: "ADT",
        offset_hours: -3,
        dst: 1,
    },
    TzName {
        name: "WAT",
        offset_hours: -1,
        dst: 0,
    },
    TzName {
        name: "GMT",
        offset_hours: 0,
        dst: 0,
    },
    TzName {
        name: "UTC",
        offset_hours: 0,
        dst: 0,
    },
    TzName {
        name: "Z",
        offset_hours: 0,
        dst: 0,
    },
    TzName {
        name: "WET",
        offset_hours: 0,
        dst: 0,
    },
    TzName {
        name: "BST",
        offset_hours: 0,
        dst: 1,
    },
    TzName {
        name: "CET",
        offset_hours: 1,
        dst: 0,
    },
    TzName {
        name: "MET",
        offset_hours: 1,
        dst: 0,
    },
    TzName {
        name: "MEWT",
        offset_hours: 1,
        dst: 0,
    },
    TzName {
        name: "MEST",
        offset_hours: 1,
        dst: 1,
    },
    TzName {
        name: "CEST",
        offset_hours: 1,
        dst: 1,
    },
    TzName {
        name: "MESZ",
        offset_hours: 1,
        dst: 1,
    },
    TzName {
        name: "FWT",
        offset_hours: 1,
        dst: 0,
    },
    TzName {
        name: "FST",
        offset_hours: 1,
        dst: 1,
    },
    TzName {
        name: "EET",
        offset_hours: 2,
        dst: 0,
    },
    TzName {
        name: "EEST",
        offset_hours: 2,
        dst: 1,
    },
    TzName {
        name: "WAST",
        offset_hours: 7,
        dst: 0,
    },
    TzName {
        name: "WADT",
        offset_hours: 7,
        dst: 1,
    },
    TzName {
        name: "CCT",
        offset_hours: 8,
        dst: 0,
    },
    TzName {
        name: "JST",
        offset_hours: 9,
        dst: 0,
    },
    TzName {
        name: "EAST",
        offset_hours: 10,
        dst: 0,
    },
    TzName {
        name: "EADT",
        offset_hours: 10,
        dst: 1,
    },
    TzName {
        name: "GST",
        offset_hours: 10,
        dst: 0,
    },
    TzName {
        name: "NZT",
        offset_hours: 12,
        dst: 0,
    },
    TzName {
        name: "NZST",
        offset_hours: 12,
        dst: 0,
    },
    TzName {
        name: "NZDT",
        offset_hours: 12,
        dst: 1,
    },
    TzName {
        name: "IDLE",
        offset_hours: 12,
        dst: 0,
    },
];

pub(crate) const MONTH_NAMES: [&str; 12] = [
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

pub(crate) const WEEKDAY_NAMES: [&str; 7] = [
    "Sundays",
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
];

/// Format a parsed instant like Git's `date_string`.
pub fn date_string(date: u64, offset: i32) -> String {
    let mut sign = '+';
    let mut o = offset;
    if o < 0 {
        o = -o;
        sign = '-';
    }
    format!("{} {}{:02}{:02}", date, sign, o / 60, o % 60)
}

/// Git `parse_date` — returns canonical `date_string` output.
pub fn parse_date(date: &str) -> Result<String, DateParseError> {
    let (ts, off) = parse_date_basic(date)?;
    Ok(date_string(ts, off))
}

/// Git `parse_date_basic` — UTC seconds and timezone offset in **minutes** (signed).
pub fn parse_date_basic(date: &str) -> Result<(u64, i32), DateParseError> {
    let bytes = date.as_bytes();
    let mut tm = init_tm_unknown();
    let mut offset: i32 = -1;
    let mut tm_gmt = 0i32;
    let mut i = 0usize;

    if bytes.first() == Some(&b'@') {
        if let Some((ts, off)) = match_object_header_date(&bytes[1..]) {
            return Ok((ts, off));
        }
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c == 0 || c == b'\n' {
            break;
        }
        let mut m = 0usize;
        if c.is_ascii_alphabetic() {
            m = match_alpha(&bytes[i..], &mut tm, &mut offset);
        } else if c.is_ascii_digit() {
            m = match_digit(&bytes[i..], &mut tm, &mut offset, &mut tm_gmt);
        } else if (c == b'-' || c == b'+') && bytes.get(i + 1).is_some_and(|x| x.is_ascii_digit()) {
            m = match_tz(&bytes[i..], &mut offset);
        }
        if m == 0 {
            m = 1;
        }
        i += m;
    }

    let tts = tm_to_time_t(&tm);
    if tts < 0 {
        return Err(DateParseError);
    }
    let mut ts = tts as u64;

    if offset == -1 {
        tm.tm_isdst = -1;
        let temp_time = unsafe { compat::mktime(&mut tm) };
        let tt = ts as i128;
        let tloc = temp_time as i128;
        offset = if tt > tloc {
            ((tt - tloc) / 60) as i32
        } else {
            -(((tloc - tt) / 60) as i32)
        };
    }

    if tm_gmt == 0 {
        if offset > 0 && (offset as i64) * 60 > ts as i64 {
            return Err(DateParseError);
        }
        if offset < 0 && (-(offset as i128)) * 60 > (TIMESTAMP_MAX as i128 - ts as i128) {
            return Err(DateParseError);
        }
        // Git: *timestamp -= *offset * 60 (signed; negative offset adds to the instant).
        let ts128 = ts as i128;
        let adj = (offset as i128) * 60;
        let new_ts = ts128 - adj;
        if new_ts < 0 {
            return Err(DateParseError);
        }
        ts = new_ts as u64;
    }

    Ok((ts, offset))
}

fn match_object_header_date(date: &[u8]) -> Option<(u64, i32)> {
    if date.is_empty() || !date[0].is_ascii_digit() {
        return None;
    }
    let (stamp, mut rest) = parse_timestamp_prefix(date);
    if rest >= date.len() || date[rest] != b' ' {
        return None;
    }
    if stamp == u64::MAX {
        return None;
    }
    rest += 1;
    if rest >= date.len() || (date[rest] != b'+' && date[rest] != b'-') {
        return None;
    }
    let sign = date[rest];
    rest += 1;
    if rest + 4 > date.len() {
        return None;
    }
    let tz_digits = std::str::from_utf8(&date[rest..rest + 4]).ok()?;
    let ofs_raw: i32 = tz_digits.parse().ok()?;
    let mut ofs = (ofs_raw / 100) * 60 + (ofs_raw % 100);
    if sign == b'-' {
        ofs = -ofs;
    }
    let end = rest + 4;
    if end < date.len() && date[end] != b'\n' && date[end] != 0 {
        return None;
    }
    Some((stamp, ofs))
}

/// Git `match_tz` — writes offset in minutes; returns bytes consumed.
fn match_tz(date: &[u8], offp: &mut i32) -> usize {
    if date.is_empty() || (date[0] != b'+' && date[0] != b'-') {
        return 0;
    }
    let (hour_ul, n) = parse_timestamp_prefix(&date[1..]);
    let mut end = 1 + n;
    let mut min: i32 = 0;
    let mut hour: i32 = hour_ul as i32;
    if n == 4 {
        min = hour % 100;
        hour /= 100;
    } else if n != 2 {
        min = 99;
    } else if end < date.len() && date[end] == b':' {
        let (m2, n2) = parse_timestamp_prefix(&date[end + 1..]);
        if n2 == 0 {
            min = 99;
        } else {
            min = m2 as i32;
            end += 1 + n2;
            if end - 1 != 5 {
                min = 99;
            }
        }
    }
    if min < 60 && hour < 24 {
        let mut off = hour * 60 + min;
        if date[0] == b'-' {
            off = -off;
        }
        *offp = off;
    }
    end
}

/// Git `strtol` for a leading signed decimal slice (`end+1` style).
fn parse_long_prefix(s: &[u8]) -> (i64, usize) {
    if s.is_empty() {
        return (0, 0);
    }
    let mut i = 0usize;
    let neg = s[0] == b'-';
    if s[0] == b'+' || s[0] == b'-' {
        i = 1;
    }
    let start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    let Ok(slice) = std::str::from_utf8(&s[start..i]) else {
        return (0, 0);
    };
    let Ok(v) = slice.parse::<i64>() else {
        return (0, 0);
    };
    let v = if neg { -v } else { v };
    (v, i)
}

fn parse_uint_suffix(s: &[u8]) -> (u64, usize) {
    parse_timestamp_prefix(s)
}

/// Git `set_date` — `0` ok, `1` reject try, `-1` error.
fn set_date(year: i32, month: i32, day: i32, now_tm: Option<&tm>, now: i64, tm: &mut tm) -> i32 {
    if !(month > 0 && month < 13 && day > 0 && day < 32) {
        return -1;
    }
    let Some(nt) = now_tm else {
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        if year == -1 {
            return 1;
        }
        if (1970..2100).contains(&year) {
            tm.tm_year = year - 1900;
        } else if (70..100).contains(&year) {
            tm.tm_year = year;
        } else if year < 38 {
            tm.tm_year = year + 100;
        } else {
            return -1;
        }
        return 0;
    };
    let mut check = *tm;
    check.tm_mon = month - 1;
    check.tm_mday = day;
    if year == -1 {
        check.tm_year = nt.tm_year;
    } else if (1970..2100).contains(&year) {
        check.tm_year = year - 1900;
    } else if (70..100).contains(&year) {
        check.tm_year = year;
    } else if year < 38 {
        check.tm_year = year + 100;
    } else {
        return -1;
    }
    let specified = tm_to_time_t(&check);
    if specified >= 0 && now + 10 * 24 * 3600 < specified {
        return -1;
    }
    tm.tm_mon = check.tm_mon;
    tm.tm_mday = check.tm_mday;
    if year != -1 {
        tm.tm_year = check.tm_year;
    }
    0
}

fn set_time(hour: i64, minute: i64, second: i64, tm: &mut tm) -> i32 {
    if (0..=24).contains(&hour) && (0..60).contains(&minute) && (0..=60).contains(&second) {
        tm.tm_hour = hour as i32;
        tm.tm_min = minute as i32;
        tm.tm_sec = second as i32;
        0
    } else {
        -1
    }
}

/// Git `match_multi_number` — `sep_i` is index of separator in `date`; returns bytes consumed from `date` start.
pub(crate) fn match_multi_number(
    num: u64,
    date: &[u8],
    sep_i: usize,
    tm: &mut tm,
    now_in: i64,
) -> usize {
    let Some(&c) = date.get(sep_i) else {
        return 0;
    };
    if !matches!(c, b':' | b'-' | b'/' | b'.') {
        return 0;
    }

    let (num2, n2) = parse_long_prefix(&date[sep_i + 1..]);
    if n2 == 0 {
        return 0;
    }
    let mut pos = sep_i + 1 + n2;
    let mut num3: i64 = -1;
    if pos < date.len() && date[pos] == c && pos + 1 < date.len() && date[pos + 1].is_ascii_digit()
    {
        let (n3, rel) = parse_long_prefix(&date[pos + 1..]);
        num3 = n3;
        pos += 1 + rel;
    }

    match c {
        b':' => {
            let mut n3 = num3;
            if n3 < 0 {
                n3 = 0;
            }
            if set_time(num as i64, num2, n3, tm) == 0 {
                if pos < date.len()
                    && date[pos] == b'.'
                    && pos + 1 < date.len()
                    && date[pos + 1].is_ascii_digit()
                    && is_date_known(tm)
                {
                    let (_, rel) = parse_long_prefix(&date[pos + 1..]);
                    pos += 1 + rel;
                }
            } else {
                return 0;
            }
        }
        b'-' | b'/' | b'.' => {
            let now = if now_in == 0 { get_time_sec() } else { now_in };
            let mut now_tm = empty_tm();
            let refuse_future: Option<&tm> = if compat::gmtime(now as time_t, &mut now_tm) {
                Some(&now_tm)
            } else {
                None
            };

            let y = num as i32;
            let m = num2 as i32;
            let d = if num3 < 0 { 0 } else { num3 as i32 };

            if num > 70 {
                if set_date(y, m, d, None, now, tm) == 0 {
                    return pos;
                }
                if set_date(y, d, m, None, now, tm) == 0 {
                    return pos;
                }
            }
            if c != b'.' && set_date(d, y, m, refuse_future, now, tm) == 0 {
                return pos;
            }
            if set_date(d, m, y, refuse_future, now, tm) == 0 {
                return pos;
            }
            if c == b'.' && set_date(d, y, m, refuse_future, now, tm) == 0 {
                return pos;
            }
            return 0;
        }
        _ => return 0,
    }
    pos
}

fn match_alpha(date: &[u8], tm: &mut tm, offset: &mut i32) -> usize {
    for (i, name) in MONTH_NAMES.iter().enumerate() {
        let m = match_string(date, name);
        if m >= 3 {
            tm.tm_mon = i as i32;
            return m;
        }
    }

    for (i, name) in WEEKDAY_NAMES.iter().enumerate() {
        let m = match_string(date, name);
        if m >= 3 {
            tm.tm_wday = i as i32;
            return m;
        }
    }

    for tz in TIMEZONE_NAMES {
        let m = match_string(date, tz.name);
        if m >= 3 || m == tz.name.len() {
            let off = tz.offset_hours + tz.dst;
            if *offset == -1 {
                *offset = 60 * off;
            }
            return m;
        }
    }

    if match_string(date, "PM") == 2 {
        tm.tm_hour = (tm.tm_hour % 12) + 12;
        return 2;
    }

    if match_string(date, "AM") == 2 {
        tm.tm_hour %= 12;
        return 2;
    }

    if date.first() == Some(&b'T')
        && date.get(1).is_some_and(|b| b.is_ascii_digit())
        && tm.tm_hour == -1
    {
        tm.tm_min = 0;
        tm.tm_sec = 0;
        return 1;
    }

    skip_alpha(date)
}

fn match_digit(date: &[u8], tm: &mut tm, offset: &mut i32, tm_gmt: &mut i32) -> usize {
    let (num, n) = parse_timestamp_prefix(date);
    if n == 0 {
        return 0;
    }
    let end = n;

    if num >= 100_000_000 && nodate(tm) && compat::gmtime(num as time_t, tm) {
        *tm_gmt = 1;
        return end;
    }

    if let Some(&sep) = date.get(end) {
        if matches!(sep, b':' | b'.' | b'/' | b'-')
            && date.get(end + 1).is_some_and(|b| b.is_ascii_digit())
        {
            let m = match_multi_number(num, date, end, tm, 0);
            if m != 0 {
                return m;
            }
        }
    }

    let mut n_digits = 0usize;
    loop {
        n_digits += 1;
        if n_digits >= date.len() || !date[n_digits].is_ascii_digit() {
            break;
        }
    }

    if n_digits == 8 || n_digits == 6 {
        let num1 = (num / 10000) as i32;
        let num2 = ((num % 10000) / 100) as i32;
        let num3 = (num % 100) as i32;
        if n_digits == 8 {
            let _ = set_date(num1, num2, num3, None, get_time_sec(), tm);
        } else if set_time(num1 as i64, num2 as i64, num3 as i64, tm) == 0
            && date.get(end) == Some(&b'.')
            && date.get(end + 1).is_some_and(|b| b.is_ascii_digit())
        {
            let (_, rel) = parse_uint_suffix(&date[end + 1..]);
            return end + 1 + rel;
        }
        return end;
    }

    if maybeiso8601(tm) {
        let mut num1 = num as u32;
        let mut num2: u32 = 0;
        if n_digits == 4 {
            num1 = (num / 100) as u32;
            num2 = (num % 100) as u32;
        }
        if (n_digits == 4 || n_digits == 2)
            && !nodate(tm)
            && set_time(num1 as i64, num2 as i64, 0, tm) == 0
        {
            return n_digits;
        }
        tm.tm_min = -1;
        tm.tm_sec = -1;
    }

    if n_digits == 4 {
        if num <= 1400 && *offset == -1 {
            let minutes = (num % 100) as u32;
            let hours = (num / 100) as u32;
            *offset = (hours * 60 + minutes) as i32;
        } else if num > 1900 && num < 2100 {
            tm.tm_year = (num as i32) - 1900;
        }
        return n_digits;
    }

    if n_digits > 2 {
        return n_digits;
    }

    if num > 0 && num < 32 && tm.tm_mday < 0 {
        tm.tm_mday = num as i32;
        return n_digits;
    }

    if n_digits == 2 && tm.tm_year < 0 {
        if num < 10 && tm.tm_mday >= 0 {
            tm.tm_year = (num as i32) + 100;
            return n_digits;
        }
        if num >= 70 {
            tm.tm_year = num as i32;
            return n_digits;
        }
    }

    if num > 0 && num < 13 && tm.tm_mon < 0 {
        tm.tm_mon = (num as i32) - 1;
    }

    n_digits
}

//! Time conversion helpers that produce Git-compatible timestamps.

use super::compat::{self, time_t, tm};

/// Unix timestamp as used by Git (`timestamp_t` is typically `uintmax_t`).
pub type Timestamp = u64;

/// Timezone in Git's signed HHMM encoding (e.g. +200 for +02:00, -500 for -05:00).
pub type TzHhmm = i32;

pub const TIMESTAMP_MAX: u64 = (((2100u64 - 1970) * 365 + 32) * 24 * 60 * 60).saturating_sub(1);

/// Convert a calendar `tm` to a Unix timestamp using only the date/time fields.
///
/// The `tm_wday` and `tm_yday` fields are ignored rather than recomputed, so the
/// result depends purely on year/month/day/hour/minute/second. Produces the same
/// values Git records for commit and author dates.
pub fn tm_to_time_t(tm: &tm) -> time_t {
    const MDAYS: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let year = tm.tm_year - 70;
    if !(0..=129).contains(&year) {
        return -1;
    }
    let month = tm.tm_mon;
    if !(0..=11).contains(&month) {
        return -1;
    }
    let mut day = tm.tm_mday;
    if month < 2 || (year + 2) % 4 != 0 {
        day -= 1;
    }
    if tm.tm_hour < 0 || tm.tm_min < 0 || tm.tm_sec < 0 {
        return -1;
    }
    let secs =
        (year as i64 * 365 + (year as i64 + 1) / 4 + MDAYS[month as usize] as i64 + day as i64)
            * 24
            * 60
            * 60
            + tm.tm_hour as i64 * 60 * 60
            + tm.tm_min as i64 * 60
            + tm.tm_sec as i64;
    secs as time_t
}

pub fn date_overflows(t: u64) -> bool {
    if t == u64::MAX {
        return true;
    }
    let sys: time_t = t as time_t;
    (t as i128) != (sys as i128) || ((t < 1) != (sys < 1))
}

/// Apply Git's `tz` HHMM encoding to a UTC instant so `gmtime_r` yields wall-clock digits.
pub fn gm_time_t(mut time: u64, tz: TzHhmm) -> Option<u64> {
    let mut minutes = if tz < 0 { -tz } else { tz };
    minutes = (minutes / 100) * 60 + (minutes % 100);
    minutes = if tz < 0 { -minutes } else { minutes };
    let adj = (minutes as i64) * 60;
    if adj > 0 {
        time = time.checked_add(adj as u64)?;
    } else if adj < 0 {
        let a = (-adj) as u64;
        if time < a {
            return None;
        }
        time -= a;
    }
    if date_overflows(time) {
        return None;
    }
    Some(time)
}

/// `time_to_tm` — UTC `tm` for display with explicit `tz` offset metadata.
pub fn time_to_tm(time: u64, tz: TzHhmm, out: &mut tm) -> bool {
    let Some(t) = gm_time_t(time, tz) else {
        return false;
    };
    compat::gmtime(t as time_t, out)
}

/// `time_to_tm_local` — `localtime_r` for the current `TZ` environment.
///
/// # Safety
///
/// `out` must point to a valid, writable `tm` for the duration of the call.
pub unsafe fn time_to_tm_local(time: u64, out: *mut tm) -> Option<*mut tm> {
    let tt = time as time_t;
    let p = compat::localtime_r(&tt, out);
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// Git's `local_time_tzoffset` — offset for `t` in the **local** zone, as HHMM encoding.
///
/// # Safety
///
/// `tm_out` must point to a valid, writable `tm` for the duration of the call.
pub unsafe fn local_time_tzoffset(t: time_t, tm_out: *mut tm) -> TzHhmm {
    let p = compat::localtime_r(&t, tm_out);
    if p.is_null() {
        return 0;
    }
    let t_local = tm_to_time_t(&*tm_out);
    if t_local == -1 {
        return 0;
    }
    let (eastwest, offset) = if (t_local as i128) < (t as i128) {
        (-1, (t as i128) - (t_local as i128))
    } else {
        (1, (t_local as i128) - (t as i128))
    };
    let mut offset_min = (offset / 60) as i32;
    offset_min = (offset_min % 60) + ((offset_min / 60) * 100);
    offset_min * eastwest
}

/// Git's `local_tzoffset` for a UTC instant (uses the process timezone).
pub fn local_tzoffset(time: u64) -> TzHhmm {
    local_tzoffset_with_tz(time, None)
}

/// Like [`local_tzoffset`] but uses an explicit `TZ`-style string when provided.
///
/// Fixed numeric offsets (`+0200`, `-0500`, `+02:00`) are parsed without touching the
/// process environment. Named zones fall back to the process timezone when unset or unknown.
#[must_use]
pub fn local_tzoffset_with_tz(time: u64, tz: Option<&str>) -> TzHhmm {
    if date_overflows(time) {
        return 0;
    }
    if let Some(tz) = tz.filter(|t| !t.is_empty()) {
        if let Some(hhmm) = parse_fixed_tz_hhmm(tz) {
            return hhmm;
        }
    }
    let t = time as time_t;
    let mut buf = std::mem::MaybeUninit::<tm>::uninit();
    unsafe {
        let tm_out = buf.as_mut_ptr();
        local_time_tzoffset(t, tm_out)
    }
}

fn parse_fixed_tz_hhmm(tz: &str) -> Option<TzHhmm> {
    let t = tz.trim();
    if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("gmt") {
        return Some(0);
    }
    let (sign, rest) = if let Some(r) = t.strip_prefix('-') {
        (-1, r)
    } else if let Some(r) = t.strip_prefix('+') {
        (1, r)
    } else {
        return None;
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    let (h, m) = match digits.len() {
        4 => (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        ),
        2 => (digits.parse::<i32>().ok()?, 0),
        _ => return None,
    };
    Some(sign * (h * 100 + m))
}

/// Current Unix time in seconds (no environment reads).
#[must_use]
pub fn system_now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse leading digits as base-10 (`strtoumax` / Git's `parse_timestamp`).
pub fn parse_timestamp_prefix(s: &[u8]) -> (u64, usize) {
    let mut i = 0usize;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return (0, 0);
    }
    let n = std::str::from_utf8(&s[..i])
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or(0);
    (n, i)
}

/// C `atoi` on a byte slice (optional leading sign for digits).
pub fn atoi_bytes(s: &[u8]) -> i32 {
    let s = trim_ascii_ws(s);
    if s.is_empty() {
        return 0;
    }
    let neg = s[0] == b'-';
    let start = if s[0] == b'+' || s[0] == b'-' { 1 } else { 0 };
    let mut v: i32 = 0;
    let mut i = start;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((s[i] - b'0') as i32);
        i += 1;
    }
    if neg {
        -v
    } else {
        v
    }
}

fn trim_ascii_ws(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t') {
        b -= 1;
    }
    &s[a..b]
}

pub fn empty_tm() -> tm {
    unsafe { std::mem::zeroed() }
}

pub fn init_tm_unknown() -> tm {
    let mut t = unsafe { std::mem::zeroed::<tm>() };
    t.tm_sec = -1;
    t.tm_min = -1;
    t.tm_hour = -1;
    t.tm_mday = -1;
    t.tm_mon = -1;
    t.tm_year = -1;
    t.tm_wday = -1;
    t.tm_yday = -1;
    t.tm_isdst = -1;
    t
}

pub fn nodate(tm: &tm) -> bool {
    (tm.tm_year & tm.tm_mon & tm.tm_mday & tm.tm_hour & tm.tm_min & tm.tm_sec) < 0
}

pub fn maybeiso8601(tm: &tm) -> bool {
    tm.tm_hour == -1 && tm.tm_min == 0 && tm.tm_sec == 0
}

pub fn is_date_known(tm: &tm) -> bool {
    tm.tm_year != -1 && tm.tm_mon != -1 && tm.tm_mday != -1
}

pub fn match_string(date: &[u8], pat: &str) -> usize {
    let pb = pat.as_bytes();
    let mut i = 0usize;
    while i < date.len() && i < pb.len() {
        let d = date[i];
        let p = pb[i];
        if d == p {
            i += 1;
            continue;
        }
        if d.eq_ignore_ascii_case(&p) {
            i += 1;
            continue;
        }
        if !d.is_ascii_alphanumeric() {
            break;
        }
        return 0;
    }
    i
}

pub fn skip_alpha(date: &[u8]) -> usize {
    let mut i = 1usize;
    while i < date.len() && date[i].is_ascii_alphabetic() {
        i += 1;
    }
    i
}

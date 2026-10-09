//! Cross-platform wrappers for C time types and functions.
//!
//! On Unix these delegate to `libc`. On Windows they use the MSVC CRT
//! equivalents (`_gmtime64_s`, `_localtime64_s`, `_mktime64`, `strftime`).

// ── types ──────────────────────────────────────────────────────────

#[cfg(unix)]
pub use libc::time_t;

#[cfg(not(unix))]
#[allow(non_camel_case_types)] // mirror the C / `libc` spelling used on Unix
pub type time_t = i64;

#[cfg(unix)]
pub use libc::tm;

/// C `struct tm` for Windows MSVC (same layout as the CRT definition).
#[cfg(not(unix))]
#[repr(C)]
#[derive(Copy, Clone)]
pub struct tm {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

// ── gmtime ─────────────────────────────────────────────────────────

/// Pure-Rust replacement for `gmtime_r`: fill `out` with the UTC broken-down
/// time for `time` (seconds since the epoch). Returns `false` when `time` is
/// outside the representable range (matching `gmtime_r` returning `NULL`).
///
/// Only the standard `struct tm` fields are written; `tm_gmtoff`/`tm_zone`
/// (where present) are left untouched. Callers route `%z`/`%Z`/`%s` through
/// `strbuf_addftime` before reaching `strftime`, so those fields are never read.
pub fn gmtime(time: time_t, out: &mut tm) -> bool {
    let Ok(dt) = ::time::OffsetDateTime::from_unix_timestamp(time) else {
        return false;
    };
    out.tm_sec = i32::from(dt.second());
    out.tm_min = i32::from(dt.minute());
    out.tm_hour = i32::from(dt.hour());
    out.tm_mday = i32::from(dt.day());
    out.tm_mon = i32::from(u8::from(dt.month())) - 1;
    out.tm_year = dt.year() - 1900;
    out.tm_wday = i32::from(dt.weekday().number_days_from_sunday());
    out.tm_yday = i32::from(dt.ordinal()) - 1;
    out.tm_isdst = 0;
    true
}

// ── localtime_r ────────────────────────────────────────────────────

#[cfg(unix)]
pub unsafe fn localtime_r(time: *const time_t, result: *mut tm) -> *mut tm {
    libc::localtime_r(time, result)
}

#[cfg(not(unix))]
pub unsafe fn localtime_r(time: *const time_t, result: *mut tm) -> *mut tm {
    unsafe extern "C" {
        fn _localtime64_s(result: *mut tm, time: *const i64) -> i32;
    }
    if unsafe { _localtime64_s(result, time) } == 0 {
        result
    } else {
        std::ptr::null_mut()
    }
}

// ── mktime ─────────────────────────────────────────────────────────

#[cfg(unix)]
pub unsafe fn mktime(tm: *mut tm) -> time_t {
    libc::mktime(tm)
}

#[cfg(not(unix))]
pub unsafe fn mktime(tm: *mut tm) -> time_t {
    unsafe extern "C" {
        fn _mktime64(tm: *mut tm) -> i64;
    }
    unsafe { _mktime64(tm) }
}

// ── strftime ───────────────────────────────────────────────────────

#[cfg(unix)]
pub unsafe fn strftime(
    buf: *mut libc::c_char,
    max: usize,
    fmt: *const libc::c_char,
    tm: *const tm,
) -> usize {
    libc::strftime(buf, max, fmt, tm)
}

#[cfg(not(unix))]
pub unsafe fn strftime(buf: *mut i8, max: usize, fmt: *const i8, tm: *const tm) -> usize {
    unsafe extern "C" {
        fn strftime(buf: *mut i8, max: usize, fmt: *const i8, tm: *const tm) -> usize;
    }
    unsafe { strftime(buf, max, fmt, tm) }
}

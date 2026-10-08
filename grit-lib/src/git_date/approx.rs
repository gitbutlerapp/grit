//! Git-compatible `approxidate` parsing.

use super::compat::{self, time_t, tm};
use super::parse::{match_multi_number, MONTH_NAMES, WEEKDAY_NAMES};
use super::tm::{match_string, parse_timestamp_prefix, process_wall_clock_sec};
use std::mem::MaybeUninit;

fn update_tm(tm: &mut tm, now: &tm, sec: i64) -> time_t {
    if tm.tm_mday < 0 {
        tm.tm_mday = now.tm_mday;
    }
    if tm.tm_mon < 0 {
        tm.tm_mon = now.tm_mon;
    }
    if tm.tm_year < 0 {
        tm.tm_year = now.tm_year;
        if tm.tm_mon > now.tm_mon {
            tm.tm_year -= 1;
        }
    }
    unsafe {
        let t = compat::mktime(tm);
        if t == -1 {
            return -1;
        }
        let n = t - sec;
        let mut out = MaybeUninit::<tm>::uninit();
        let p = compat::localtime_r(&n, out.as_mut_ptr());
        if p.is_null() {
            return -1;
        }
        *tm = *p;
        n
    }
}

fn pending_number(tm: &mut tm, num: &mut i32) {
    let number = *num;
    if number != 0 {
        *num = 0;
        if tm.tm_mday < 0 && number < 32 {
            tm.tm_mday = number;
        } else if tm.tm_mon < 0 && number < 13 {
            tm.tm_mon = number - 1;
        } else if tm.tm_year < 0 {
            if (1969..2100).contains(&number) {
                tm.tm_year = number - 1900;
            } else if (69..100).contains(&number) {
                tm.tm_year = number;
            } else if number < 38 {
                tm.tm_year = number + 100;
            }
        }
    }
}

fn date_now(tm: &mut tm, now: &tm, num: &mut i32) {
    *num = 0;
    let _ = update_tm(tm, now, 0);
}

fn date_yesterday(tm: &mut tm, now: &tm, num: &mut i32) {
    *num = 0;
    let _ = update_tm(tm, now, 24 * 60 * 60);
}

fn date_time(tm: &mut tm, now: &tm, hour: i32) {
    if tm.tm_hour < hour {
        let _ = update_tm(tm, now, 24 * 60 * 60);
    }
    tm.tm_hour = hour;
    tm.tm_min = 0;
    tm.tm_sec = 0;
}

fn date_midnight(tm: &mut tm, now: &tm, num: &mut i32) {
    pending_number(tm, num);
    date_time(tm, now, 0);
}

fn date_noon(tm: &mut tm, now: &tm, num: &mut i32) {
    pending_number(tm, num);
    date_time(tm, now, 12);
}

fn date_tea(tm: &mut tm, now: &tm, num: &mut i32) {
    pending_number(tm, num);
    date_time(tm, now, 17);
}

fn date_pm(tm: &mut tm, _now: &tm, num: &mut i32) {
    let n = *num;
    *num = 0;
    let mut hour = tm.tm_hour;
    if n != 0 {
        hour = n;
        tm.tm_min = 0;
        tm.tm_sec = 0;
    }
    tm.tm_hour = (hour % 12) + 12;
}

fn date_am(tm: &mut tm, _now: &tm, num: &mut i32) {
    let n = *num;
    *num = 0;
    let mut hour = tm.tm_hour;
    if n != 0 {
        hour = n;
        tm.tm_min = 0;
        tm.tm_sec = 0;
    }
    tm.tm_hour = hour % 12;
}

fn date_never(tm: &mut tm, _now: &tm, num: &mut i32) {
    let n: time_t = 0;
    unsafe {
        let mut out = MaybeUninit::<tm>::uninit();
        let p = compat::localtime_r(&n, out.as_mut_ptr());
        if !p.is_null() {
            *tm = *p;
        }
    }
    *num = 0;
}

const NUMBER_NAME: [&str; 11] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];

const TYPELEN: [(&str, i64); 5] = [
    ("seconds", 1),
    ("minutes", 60),
    ("hours", 60 * 60),
    ("days", 24 * 60 * 60),
    ("weeks", 7 * 24 * 60 * 60),
];

fn alpha_word_len(date: &[u8]) -> usize {
    let mut i = 0usize;
    while i < date.len() && date[i].is_ascii_alphabetic() {
        i += 1;
    }
    i
}

fn approxidate_alpha(
    date: &[u8],
    tm: &mut tm,
    now: &tm,
    num: &mut i32,
    touched: &mut i32,
) -> usize {
    let end = alpha_word_len(date);

    for (i, name) in MONTH_NAMES.iter().enumerate() {
        let m = match_string(date, name);
        if m >= 3 {
            tm.tm_mon = i as i32;
            *touched = 1;
            return end;
        }
    }

    type SpecialDateHandler = (&'static str, fn(&mut tm, &tm, &mut i32));
    let specials: [SpecialDateHandler; 8] = [
        ("yesterday", date_yesterday),
        ("noon", date_noon),
        ("midnight", date_midnight),
        ("tea", date_tea),
        ("PM", date_pm),
        ("AM", date_am),
        ("never", date_never),
        ("now", date_now),
    ];
    for (name, f) in specials {
        if match_string(date, name) == name.len() {
            f(tm, now, num);
            *touched = 1;
            return end;
        }
    }

    if *num == 0 {
        for (i, name) in NUMBER_NAME.iter().enumerate().skip(1).take(10) {
            let len = name.len();
            if match_string(date, name) == len {
                *num = i as i32;
                *touched = 1;
                return end;
            }
        }
        if match_string(date, "last") == 4 {
            *num = 1;
            *touched = 1;
        }
        return end;
    }

    for (typ, len_secs) in TYPELEN {
        let tlen = typ.len();
        if match_string(date, typ) >= tlen.saturating_sub(1) {
            let _ = update_tm(tm, now, len_secs * (*num as i64));
            *num = 0;
            *touched = 1;
            return end;
        }
    }

    for (i, wname) in WEEKDAY_NAMES.iter().enumerate() {
        let m = match_string(date, wname);
        if m >= 3 {
            let mut n = *num - 1;
            *num = 0;
            let mut diff = tm.tm_wday - (i as i32);
            if diff <= 0 {
                n += 1;
            }
            diff += 7 * n;
            let _ = update_tm(tm, now, (diff as i64) * 24 * 60 * 60);
            *touched = 1;
            return end;
        }
    }

    if match_string(date, "months") >= 5 {
        let _ = update_tm(tm, now, 0);
        let mut n = tm.tm_mon - *num;
        *num = 0;
        while n < 0 {
            n += 12;
            tm.tm_year -= 1;
        }
        tm.tm_mon = n;
        *touched = 1;
        return end;
    }

    if match_string(date, "years") >= 4 {
        let _ = update_tm(tm, now, 0);
        tm.tm_year -= *num;
        *num = 0;
        *touched = 1;
        return end;
    }

    end
}

fn approxidate_digit(date: &[u8], tm: &mut tm, num: &mut i32, now_sec: i64) -> usize {
    let (number, n) = parse_timestamp_prefix(date);
    if n == 0 {
        return 0;
    }
    let end = n;

    if let Some(&sep) = date.get(end) {
        if matches!(sep, b':' | b'.' | b'/' | b'-')
            && date.get(end + 1).is_some_and(|b| b.is_ascii_digit())
        {
            let m = match_multi_number(number, date, end, tm, now_sec);
            if m != 0 {
                return m;
            }
        }
    }

    if date[0] != b'0' || end <= 2 {
        *num = number as i32;
    }
    end
}

/// Git `approxidate_careful` — returns Unix timestamp; on parse failure uses fuzzy parser.
pub fn approxidate_careful(date: &str, error_ret: Option<&mut i32>) -> u64 {
    approxidate_careful_at(date, process_wall_clock_sec(), error_ret)
}

/// Like [`approxidate_careful`] but uses `now_sec` for relative fields.
pub fn approxidate_careful_at(date: &str, now_sec: i64, error_ret: Option<&mut i32>) -> u64 {
    let mut dummy = 0;
    let er: &mut i32 = match error_ret {
        Some(p) => p,
        None => &mut dummy,
    };
    if let Ok((ts, _)) = super::parse::parse_date_basic_at(date, now_sec) {
        *er = 0;
        return ts;
    }
    approxidate_str(date, now_sec, er)
}

fn approxidate_str(date: &str, time_sec: i64, error_ret: &mut i32) -> u64 {
    let mut tm_buf = MaybeUninit::<tm>::uninit();
    let mut now_buf = MaybeUninit::<tm>::uninit();
    unsafe {
        let tt = time_sec as time_t;
        let p = compat::localtime_r(&tt, tm_buf.as_mut_ptr());
        if p.is_null() {
            *error_ret = 1;
            return 0;
        }
        let p2 = compat::localtime_r(&tt, now_buf.as_mut_ptr());
        if p2.is_null() {
            *error_ret = 1;
            return 0;
        }
        let mut tm = *tm_buf.as_ptr();
        let now = *now_buf.as_ptr();
        tm.tm_year = -1;
        tm.tm_mon = -1;
        tm.tm_mday = -1;

        let mut number = 0i32;
        let mut touched = 0i32;
        let bytes = date.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i];
            if c == 0 || c == b'\n' {
                break;
            }
            if c.is_ascii_digit() {
                pending_number(&mut tm, &mut number);
                let adv = approxidate_digit(&bytes[i..], &mut tm, &mut number, time_sec);
                if adv == 0 {
                    break;
                }
                i += adv;
                touched = 1;
                continue;
            }
            if c.is_ascii_alphabetic() {
                let adv = approxidate_alpha(&bytes[i..], &mut tm, &now, &mut number, &mut touched);
                i += adv;
                continue;
            }
            i += 1;
        }
        pending_number(&mut tm, &mut number);
        if touched == 0 {
            *error_ret = 1;
        }
        let n = update_tm(&mut tm, &now, 0);
        if n < 0 {
            *error_ret = 1;
            return 0;
        }
        n as u64
    }
}

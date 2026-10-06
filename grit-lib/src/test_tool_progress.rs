//! `test-tool progress` — exercises Git-compatible progress display (`t0500`).
//!
//! Mirrors `git/t/helper/test-progress.c` and `git/progress.c` with `GIT_TEST_PROGRESS_ONLY`
//! behavior (no SIGALRM; fake elapsed time via `throughput` lines setting `progress_test_ns`).

use std::io::{self, BufRead, Write};

use unicode_width::UnicodeWidthStr;

use crate::diffstat::terminal_columns;

const TP_IDX_MAX: usize = 8;

thread_local! {
    static PROGRESS_TEST_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn utf8_display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Match `humanise_bytes()` in `git/strbuf.c` (non-rate).
fn humanise_bytes_value_unit(bytes: u64, rate: bool) -> (String, &'static str) {
    if bytes > 1 << 30 {
        let value = format!(
            "{}.{:02}",
            bytes >> 30,
            (bytes & ((1 << 30) - 1)) / 10_737_419
        );
        let unit = if rate { "GiB/s" } else { "GiB" };
        (value, unit)
    } else if bytes > 1 << 20 {
        let x = bytes + 5243;
        let value = format!("{}.{:02}", x >> 20, ((x & ((1 << 20) - 1)) * 100) >> 20);
        let unit = if rate { "MiB/s" } else { "MiB" };
        (value, unit)
    } else if bytes > 1 << 10 {
        let x = bytes + 5;
        let value = format!("{}.{:02}", x >> 10, ((x & ((1 << 10) - 1)) * 100) >> 10);
        let unit = if rate { "KiB/s" } else { "KiB" };
        (value, unit)
    } else {
        let value = format!("{bytes}");
        let unit = if rate {
            if bytes == 1 {
                "byte/s"
            } else {
                "bytes/s"
            }
        } else if bytes == 1 {
            "byte"
        } else {
            "bytes"
        };
        (value, unit)
    }
}

fn throughput_display(total: u64, rate: u32) -> String {
    let (v1, u1) = humanise_bytes_value_unit(total, false);
    let (v2, u2) = humanise_bytes_value_unit(u64::from(rate).saturating_mul(1024), true);
    format!(", {v1} {u1} | {v2} {u2}")
}

struct Throughput {
    curr_total: u64,
    prev_total: u64,
    prev_ns: u64,
    avg_bytes: u64,
    avg_misecs: u64,
    last_bytes: [u32; TP_IDX_MAX],
    last_misecs: [u32; TP_IDX_MAX],
    idx: usize,
    display: String,
}

impl Throughput {
    fn new(byte_count: u64, now_ns: u64) -> Self {
        Self {
            curr_total: byte_count,
            prev_total: byte_count,
            prev_ns: now_ns,
            avg_bytes: 0,
            avg_misecs: 0,
            last_bytes: [0; TP_IDX_MAX],
            last_misecs: [0; TP_IDX_MAX],
            idx: 0,
            display: String::new(),
        }
    }
}

struct Progress {
    title: String,
    last_value: Option<u64>,
    total: u64,
    last_percent: Option<u32>,
    throughput: Option<Throughput>,
    start_ns: u64,
    counters_sb: String,
    title_len: usize,
    split: bool,
    force_update: bool,
}

impl Progress {
    fn new(title: String, total: u64) -> Self {
        let title_len = utf8_display_width(&title);
        let start_ns = nanotime();
        Self {
            title,
            last_value: None,
            total,
            last_percent: None,
            throughput: None,
            start_ns,
            counters_sb: String::new(),
            title_len,
            split: false,
            force_update: false,
        }
    }

    fn now_ns(&self) -> u64 {
        self.start_ns.saturating_add(PROGRESS_TEST_NS.get())
    }

    fn render_line(&mut self, n: u64, done_suffix: Option<&str>) -> io::Result<()> {
        let mut show_update = false;
        let update = self.force_update;
        self.force_update = false;

        let last_count_len = self.counters_sb.len();
        self.last_value = Some(n);

        let tp = self
            .throughput
            .as_ref()
            .map(|t| t.display.as_str())
            .unwrap_or("");

        if self.total > 0 {
            let percent: u32 = ((n as u128 * 100) / u128::from(self.total)) as u32;
            if Some(percent) != self.last_percent || update {
                self.last_percent = Some(percent);
                self.counters_sb = format!("{:3}% ({n}/{}){tp}", percent, self.total);
                show_update = true;
            }
        } else if update {
            self.counters_sb = format!("{n}{tp}");
            show_update = true;
        }

        if !show_update {
            return Ok(());
        }

        let stderr = io::stderr();
        let show = is_foreground_stderr(&stderr) || done_suffix.is_some();
        if !show {
            return Ok(());
        }

        let eol = done_suffix.unwrap_or("\r");
        let clear_len = if self.counters_sb.len() < last_count_len {
            last_count_len - self.counters_sb.len() + 1
        } else {
            0
        };
        let progress_line_len = self.title_len + self.counters_sb.len() + 2;
        let cols = terminal_columns();

        let mut err = stderr.lock();
        if self.split {
            // Git: `fprintf(stderr, "  %s%*s", counters_sb->buf, (int) clear_len, eol);`
            let w = clear_len.max(eol.len());
            write!(err, "  {}{:>w$}", self.counters_sb, eol, w = w)?;
        } else if done_suffix.is_none() && cols < progress_line_len {
            let title_pad = if self.title_len + 1 < cols {
                cols - self.title_len - 1
            } else {
                0
            };
            write!(
                err,
                "{}:{}\n  {}{:>w$}",
                self.title,
                " ".repeat(title_pad),
                self.counters_sb,
                eol,
                w = clear_len.max(eol.len())
            )?;
            self.split = true;
        } else {
            write!(
                err,
                "{}: {}{:>w$}",
                self.title,
                self.counters_sb,
                eol,
                w = clear_len.max(eol.len())
            )?;
        }
        err.flush()?;
        Ok(())
    }

    fn display_progress(&mut self, n: u64) -> io::Result<()> {
        self.render_line(n, None)
    }

    fn display_throughput(&mut self, total: u64, global_update: bool) -> io::Result<()> {
        let now_ns = self.now_ns();

        if self.throughput.is_none() {
            self.throughput = Some(Throughput::new(total, now_ns));
            return Ok(());
        }
        let Some(tp) = self.throughput.as_mut() else {
            return Ok(());
        };
        tp.curr_total = total;

        if now_ns.saturating_sub(tp.prev_ns) <= 500_000_000 {
            return Ok(());
        }

        let misecs: u32 = (((now_ns - tp.prev_ns) as u128 * 4398) >> 32) as u32;
        let count = total.saturating_sub(tp.prev_total);
        tp.prev_total = total;
        tp.prev_ns = now_ns;
        tp.avg_bytes = tp.avg_bytes.saturating_add(count);
        tp.avg_misecs = tp.avg_misecs.saturating_add(u64::from(misecs));
        let rate = tp
            .avg_bytes
            .checked_div(tp.avg_misecs)
            .unwrap_or(0) as u32;
        tp.avg_bytes = tp
            .avg_bytes
            .saturating_sub(u64::from(tp.last_bytes[tp.idx]));
        tp.avg_misecs = tp
            .avg_misecs
            .saturating_sub(u64::from(tp.last_misecs[tp.idx]));
        tp.last_bytes[tp.idx] = count as u32;
        tp.last_misecs[tp.idx] = misecs;
        tp.idx = (tp.idx + 1) % TP_IDX_MAX;

        tp.display = throughput_display(total, rate);

        if self.last_value.is_some() && global_update {
            let n = self.last_value.unwrap_or(0);
            self.force_update = true;
            self.display_progress(n)?;
        }
        Ok(())
    }

    fn force_last_update(&mut self, msg: &str) -> io::Result<()> {
        let now_ns = self.now_ns();
        if let Some(tp) = self.throughput.as_mut() {
            let misecs: u32 =
                (((now_ns.saturating_sub(self.start_ns)) as u128 * 4398) >> 32) as u32;
            let rate = if misecs > 0 {
                (tp.curr_total / u64::from(misecs)) as u32
            } else {
                0
            };
            tp.display = throughput_display(tp.curr_total, rate);
        }
        self.force_update = true;
        let n = self.last_value.unwrap_or(0);
        let done = format!(", {msg}.\n");
        self.render_line(n, Some(&done))?;
        Ok(())
    }

    fn stop(&mut self, trace_path: Option<&str>) -> io::Result<()> {
        if self.last_value.is_some() {
            self.force_last_update("done")?;
        }
        if let Some(path) = trace_path {
            trace2_append_json_line(
                path,
                &format!(
                    r#"{{"event":"data","sid":"grit-0","time":"{}","category":"progress","key":"total_objects","value":"{}"}}"#,
                    trace_now(),
                    self.total
                ),
            )?;
            if let Some(tp) = &self.throughput {
                trace2_append_json_line(
                    path,
                    &format!(
                        r#"{{"event":"data","sid":"grit-0","time":"{}","category":"progress","key":"total_bytes","value":"{}"}}"#,
                        trace_now(),
                        tp.curr_total
                    ),
                )?;
            }
            trace2_append_json_line(
                path,
                &format!(
                    r#"{{"event":"region_leave","sid":"grit-0","time":"{}","category":"progress","label":"{}","t_rel":0.0}}"#,
                    trace_now(),
                    json_escape(&self.title)
                ),
            )?;
        }
        Ok(())
    }
}

fn nanotime() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn trace_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let micros = now.subsec_micros();
    let secs_in_day = total_secs % 86400;
    let hours = secs_in_day / 3600;
    let mins = (secs_in_day % 3600) / 60;
    let secs = secs_in_day % 60;
    format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn trace2_append_json_line(path: &str, line: &str) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(unix)]
fn is_foreground_stderr(stderr: &io::Stderr) -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = stderr.as_raw_fd();
    // SAFETY: libc tcgetpgrp/getpgid match Git `is_foreground_fd`.
    unsafe {
        let tpgrp = libc::tcgetpgrp(fd);
        if tpgrp < 0 {
            return true;
        }
        libc::getpgid(0) == tpgrp
    }
}

#[cfg(not(unix))]
fn is_foreground_stderr(_stderr: &io::Stderr) -> bool {
    true
}

/// Run `test-tool progress`: read script lines from stdin, write progress to stderr.
///
/// When `GIT_TRACE2_EVENT` is set to a path, emits `region_enter` / `data` / `region_leave` lines
/// compatible with `test_region` and t0500 greps.
pub fn run() -> io::Result<()> {
    PROGRESS_TEST_NS.set(0);

    let trace_path = std::env::var("GIT_TRACE2_EVENT")
        .ok()
        .filter(|s| !s.is_empty());

    let stdin = io::stdin();
    let mut progress: Option<Progress> = None;
    let mut title_storage: Vec<String> = Vec::new();

    for line in stdin.lock().lines() {
        let line = line?;
        if let Some(rest) = line.strip_prefix("start ") {
            let mut parts = rest.splitn(2, |c: char| c.is_ascii_whitespace());
            let total_str = parts.next().unwrap_or("");
            let total: u64 = total_str.parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid start total: {e}"),
                )
            })?;
            let title: String = match parts.next() {
                None | Some("") => "Working hard".to_string(),
                Some(t) => {
                    title_storage.push(t.to_string());
                    t.to_string()
                }
            };
            if let Some(path) = trace_path.as_deref() {
                trace2_append_json_line(
                    path,
                    &format!(
                        r#"{{"event":"region_enter","sid":"grit-0","time":"{}","category":"progress","label":"{}"}}"#,
                        trace_now(),
                        json_escape(&title)
                    ),
                )?;
            }
            progress = Some(Progress::new(title, total));
        } else if let Some(rest) = line.strip_prefix("progress ") {
            let n: u64 = rest.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid progress value: {e}"),
                )
            })?;
            if let Some(ref mut p) = progress {
                p.display_progress(n)?;
            }
        } else if let Some(rest) = line.strip_prefix("throughput ") {
            let mut it = rest.split_whitespace();
            let byte_count: u64 = it
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "throughput: missing bytes")
                })?
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
            let test_ms: u64 = it
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "throughput: missing millis")
                })?
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
            PROGRESS_TEST_NS.set(test_ms.saturating_mul(1_000_000));
            let global_update = progress.as_ref().is_some_and(|p| p.force_update);
            if let Some(ref mut p) = progress {
                p.display_throughput(byte_count, global_update)?;
            }
        } else if line == "update" {
            if let Some(ref mut p) = progress {
                p.force_update = true;
            }
        } else if line == "stop" {
            if let Some(mut p) = progress.take() {
                p.stop(trace_path.as_deref())?;
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid input: '{line}'"),
            ));
        }
    }

    Ok(())
}

//! The resumable-transfer display: a clone bundle download or a push bundle
//! upload, drawn differently from the per-phase lines so it is clear the
//! transfer can be picked up again.
//!
//! ```text
//! ⇣ Downloading bundle  host/repo/bundles/1700000000-ab12cd.bundle
//!   ━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────  64%  77.0 MiB of 120 MiB  12.4 MiB/s  3s left
//! ```
//!
//! When an earlier, interrupted run already moved part of the file, the header
//! says so and the bar shows that part in a different color from what this run
//! moved. A dropped connection gets its own line before the transfer carries on.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use grit_lib::remote::resumable::{ResumableEvent, TransferDirection};

use super::{
    format_bytes, format_duration, format_eta, format_rate, paint, terminal_width, BOLD, DIM,
};

/// Cells in the bar.
const BAR_WIDTH: usize = 32;
/// Minimum time between redraws.
const MIN_REDRAW: Duration = Duration::from_millis(80);

const DOWNLOAD: &str = "38;5;177";
const UPLOAD: &str = "38;5;75";
const RESUMED: &str = "38;5;242";
const YELLOW: &str = "33";
const GREEN: &str = "32";

/// A finished resumable transfer.
#[derive(Debug, Clone, PartialEq)]
struct ResumableRecord {
    /// Download or upload.
    pub direction: TransferDirection,
    /// Total bytes.
    pub bytes: u64,
    /// Bytes an earlier run had already moved.
    pub resumed_from: u64,
    /// Connections that dropped and were picked up again in this run.
    pub reconnects: u32,
    /// Time this run spent on it.
    pub elapsed: Duration,
}

struct Active {
    direction: TransferDirection,
    total: Option<u64>,
    resumed_from: u64,
    done: u64,
    started: Instant,
    reconnects: u32,
}

/// Draws [`ResumableEvent`]s on stderr (when `draw`).
pub struct ResumableDisplay {
    draw: bool,
    color: bool,
    active: Option<Active>,
    last_draw: Instant,
    frame: usize,
}

impl ResumableDisplay {
    /// A display that draws only when `draw` (stderr is a terminal in human mode).
    #[must_use]
    pub fn new(draw: bool, color: bool) -> Self {
        Self {
            draw,
            color,
            active: None,
            last_draw: Instant::now() - MIN_REDRAW,
            frame: 0,
        }
    }

    /// Whether a transfer is on screen.
    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Show `event`.
    pub fn handle(&mut self, event: &ResumableEvent) {
        match event {
            ResumableEvent::Started {
                direction,
                url,
                total,
                resumed_from,
            } => {
                self.active = Some(Active {
                    direction: *direction,
                    total: *total,
                    resumed_from: *resumed_from,
                    done: *resumed_from,
                    started: Instant::now(),
                    reconnects: 0,
                });
                let header = header_line(*direction, url, *total, *resumed_from, self.color);
                self.print_line(&header);
                self.redraw(true);
            }
            ResumableEvent::Advanced { done, total } => {
                if let Some(active) = &mut self.active {
                    active.done = *done;
                    active.total = total.or(active.total);
                }
                self.redraw(false);
            }
            ResumableEvent::Reconnecting { done, attempt } => {
                if let Some(active) = &mut self.active {
                    active.done = *done;
                    active.reconnects = *attempt;
                }
                let line = format!(
                    "  {} connection dropped at {}; picking up from there (retry {attempt})",
                    paint(self.color, YELLOW, "⚠"),
                    format_bytes(*done),
                );
                self.print_line(&line);
                self.redraw(true);
            }
            ResumableEvent::Finished { done, resumed_from } => {
                let Some(active) = self.active.take() else {
                    return;
                };
                let record = ResumableRecord {
                    direction: active.direction,
                    bytes: *done,
                    resumed_from: *resumed_from,
                    reconnects: active.reconnects,
                    elapsed: active.started.elapsed(),
                };
                let line = done_line(&record, self.color);
                self.print_line(&line);
            }
        }
    }

    /// Replace the live line with `line` and move to the next row.
    fn print_line(&self, line: &str) {
        if !self.draw {
            return;
        }
        let mut err = io::stderr();
        let _ = write!(err, "\r{}{line}\n", clear(self.color));
        let _ = err.flush();
    }

    fn redraw(&mut self, force: bool) {
        if !self.draw {
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let now = Instant::now();
        if !force && now.duration_since(self.last_draw) < MIN_REDRAW {
            return;
        }
        self.frame = self.frame.wrapping_add(1);
        let line = live_line(active, now, self.frame, terminal_width(), self.color);
        let mut err = io::stderr();
        let _ = write!(err, "\r{}{line}", clear(self.color));
        let _ = err.flush();
        self.last_draw = now;
    }
}

fn clear(color: bool) -> String {
    if color {
        "\x1b[2K".to_owned()
    } else {
        format!("{}\r", " ".repeat(terminal_width().saturating_sub(1)))
    }
}

fn accent(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => DOWNLOAD,
        TransferDirection::Upload => UPLOAD,
    }
}

fn noun(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => "bundle",
        TransferDirection::Upload => "push bundle",
    }
}

/// `host/path` of `url`, without the scheme or credentials.
fn short_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = path.split(['?', '#']).next().unwrap_or_default();
    format!("{host}{path}")
}

fn header_line(
    direction: TransferDirection,
    url: &str,
    total: Option<u64>,
    resumed_from: u64,
    color: bool,
) -> String {
    let verb = match direction {
        TransferDirection::Download => "Downloading",
        TransferDirection::Upload => "Uploading",
    };
    if resumed_from == 0 {
        let size = total
            .map(|t| format!(" ({})", format_bytes(t)))
            .unwrap_or_default();
        return format!(
            "{} {}{}  {}",
            paint(color, accent(direction), arrow(direction)),
            paint(color, BOLD, &format!("{verb} {}", noun(direction))),
            size,
            paint(color, DIM, &short_url(url)),
        );
    }
    let share = total
        .filter(|t| *t > 0)
        .map(|t| {
            format!(
                " of {} ({:.0}%)",
                format_bytes(t),
                resumed_from as f64 * 100.0 / t as f64
            )
        })
        .unwrap_or_default();
    let what = match direction {
        TransferDirection::Download => "bundle download",
        TransferDirection::Upload => "push bundle upload",
    };
    format!(
        "{} {} at {}{share}  {}",
        paint(color, YELLOW, "↻"),
        paint(color, BOLD, &format!("Resuming {what}")),
        format_bytes(resumed_from),
        paint(color, DIM, &short_url(url)),
    )
}

fn arrow(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => "⇣",
        TransferDirection::Upload => "⇡",
    }
}

/// The bar: what an earlier run moved, what this run moved, what is left.
fn bar(active: &Active, color: bool) -> String {
    let Some(total) = active.total.filter(|t| *t > 0) else {
        return String::new();
    };
    let cells =
        |bytes: u64| ((bytes.min(total) as f64 / total as f64) * BAR_WIDTH as f64).round() as usize;
    let old = cells(active.resumed_from);
    let done = cells(active.done).max(old);
    let new = done - old;
    let left = BAR_WIDTH - done;
    if color {
        format!(
            "{}{}{}",
            paint(true, RESUMED, &"━".repeat(old)),
            paint(true, accent(active.direction), &"━".repeat(new)),
            paint(true, DIM, &"─".repeat(left)),
        )
    } else {
        format!(
            "[{}{}{}]",
            "=".repeat(old),
            "#".repeat(new),
            " ".repeat(left)
        )
    }
}

fn live_line(active: &Active, now: Instant, frame: usize, width: usize, color: bool) -> String {
    let elapsed = now.duration_since(active.started);
    let moved = active.done.saturating_sub(active.resumed_from);
    let speed = if elapsed.as_secs_f64() > 0.0 {
        moved as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    let mut parts = Vec::new();
    match active.total.filter(|t| *t > 0) {
        Some(total) => {
            parts.push(format!(
                "{:>3}%",
                (active.done.min(total) as f64 * 100.0 / total as f64) as u64
            ));
            parts.push(format!(
                "{} of {}",
                format_bytes(active.done),
                format_bytes(total)
            ));
            parts.push(format_rate(speed));
            if speed > 0.0 && active.done < total {
                let left = Duration::from_secs_f64((total - active.done) as f64 / speed);
                parts.push(format_eta(left));
            }
        }
        None => {
            const SPIN: [char; 4] = ['◐', '◓', '◑', '◒'];
            parts.push(SPIN[frame % SPIN.len()].to_string());
            parts.push(format_bytes(active.done));
            parts.push(format_rate(speed));
        }
    }
    let stats = parts.join("  ");
    let line = format!("  {}  {stats}", bar(active, color));
    // Drop the bar on very narrow terminals rather than wrapping.
    if width < BAR_WIDTH + stats.chars().count() + 6 {
        return format!("  {stats}");
    }
    line
}

fn done_line(record: &ResumableRecord, color: bool) -> String {
    let moved = record.bytes.saturating_sub(record.resumed_from);
    let speed = if record.elapsed.as_secs_f64() > 0.0 {
        moved as f64 / record.elapsed.as_secs_f64()
    } else {
        0.0
    };
    let mut parts = vec![
        format!(
            "{} {} in {}",
            noun(record.direction),
            paint(color, BOLD, &format_bytes(record.bytes)),
            format_duration(record.elapsed)
        ),
        format_rate(speed),
    ];
    if record.resumed_from > 0 {
        parts.push(format!("resumed at {}", format_bytes(record.resumed_from)));
    }
    if record.reconnects > 0 {
        let s = if record.reconnects == 1 { "" } else { "s" };
        parts.push(format!("{} reconnect{s}", record.reconnects));
    }
    format!(
        "  {} {}",
        paint(color, GREEN, "✓"),
        parts.join(&paint(color, DIM, " · "))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn active(resumed_from: u64, done: u64) -> Active {
        Active {
            direction: TransferDirection::Download,
            total: Some(100 * 1024 * 1024),
            resumed_from,
            done,
            started: Instant::now() - Duration::from_secs(2),
            reconnects: 0,
        }
    }

    #[test]
    fn headers_say_whether_a_transfer_resumes() {
        let fresh = header_line(
            TransferDirection::Download,
            "http://u:p@h:8/r/bundles/1-a.bundle",
            Some(1024),
            0,
            true,
        );
        assert_eq!(
            visible(&fresh),
            "⇣ Downloading bundle (1.00 KiB)  h:8/r/bundles/1-a.bundle"
        );
        let resumed = header_line(
            TransferDirection::Upload,
            "http://h/r/push/1",
            Some(400),
            100,
            false,
        );
        assert!(
            resumed.starts_with("↻ Resuming push bundle upload"),
            "{resumed}"
        );
        assert!(resumed.contains("at 100 B of 400 B (25%)"), "{resumed}");
    }

    #[test]
    fn the_bar_tells_resumed_bytes_from_new_ones() {
        let plain = bar(&active(25 * 1024 * 1024, 50 * 1024 * 1024), false);
        assert_eq!(
            plain,
            format!("[{}{}{}]", "=".repeat(8), "#".repeat(8), " ".repeat(16))
        );
        let colored = bar(&active(25 * 1024 * 1024, 50 * 1024 * 1024), true);
        assert_eq!(visible(&colored).chars().count(), BAR_WIDTH);
        assert!(colored.contains(RESUMED) && colored.contains(DOWNLOAD));
    }

    #[test]
    fn live_lines_show_share_size_rate_and_time_left() {
        let line = visible(&live_line(
            &active(0, 50 * 1024 * 1024),
            Instant::now(),
            0,
            120,
            true,
        ));
        assert!(line.contains(" 50%"), "{line}");
        assert!(line.contains("50.0 MiB of 100 MiB"), "{line}");
        assert!(
            line.contains("s left") && !line.contains("left left"),
            "{line}"
        );
        let narrow = visible(&live_line(&active(0, 1), Instant::now(), 0, 30, true));
        assert!(!narrow.contains('━') && !narrow.contains('─'), "{narrow}");
    }

    #[test]
    fn done_lines_mention_resumes_and_reconnects() {
        let record = ResumableRecord {
            direction: TransferDirection::Download,
            bytes: 2048,
            resumed_from: 1024,
            reconnects: 2,
            elapsed: Duration::from_secs(1),
        };
        let line = visible(&done_line(&record, true));
        assert!(line.starts_with("  ✓ bundle 2.00 KiB in "), "{line}");
        assert!(line.contains("resumed at 1.00 KiB"), "{line}");
        assert!(line.ends_with("2 reconnects"), "{line}");
    }

    #[test]
    fn transfers_start_and_finish_off_a_terminal() {
        let mut display = ResumableDisplay::new(false, false);
        display.handle(&ResumableEvent::Started {
            direction: TransferDirection::Upload,
            url: "http://h/r/push/1".to_owned(),
            total: Some(10),
            resumed_from: 4,
        });
        assert!(display.is_active());
        display.handle(&ResumableEvent::Finished {
            done: 10,
            resumed_from: 4,
        });
        assert!(!display.is_active());
    }
}

//! Terminal progress for fetch, pull and clone: one live line per phase with a
//! scrolling, colored throughput graph, kept on screen as a summary line when
//! the phase finishes.
//!
//! Phase timings are recorded even when nothing is drawn (non-TTY stderr,
//! `--json`, `--markdown`) so commands can report them in their outcome.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use grit_lib::progress::{ProgressPhase, ProgressSink, ProgressUnit};
use grit_lib::remote::fetch::Progress;
use grit_lib::remote::sideband_lines::{RemoteLine, SidebandLines};
use terminal_size::{terminal_size, Width};

use crate::output::OutputMode;

/// Minimum time between in-place redraws.
const MIN_REDRAW: Duration = Duration::from_millis(80);
/// How often a throughput sample is taken for the graph.
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
/// Graph width bounds, in terminal cells.
const GRAPH_MAX: usize = 28;
const GRAPH_MIN: usize = 8;
/// Cells in the live completion bar for phases with a known total.
const BAR_WIDTH: usize = 10;
/// Width of the phase label column.
const LABEL_WIDTH: usize = 9;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Timing and throughput for one finished phase.
#[derive(Debug, Clone)]
pub struct PhaseRecord {
    /// Which phase this was.
    pub phase: ProgressPhase,
    /// Unit of [`Self::count`].
    pub unit: ProgressUnit,
    /// Units processed (bytes, objects or files).
    pub count: u64,
    /// When the phase started, measured from the sink's creation.
    pub started_at: Duration,
    /// Wall-clock duration of the phase.
    pub elapsed: Duration,
    /// Throughput samples in units per second, one per sampling interval.
    pub samples: Vec<f64>,
    /// Highest sampled throughput.
    pub peak_rate: f64,
}

impl PhaseRecord {
    /// Average throughput over the whole phase, in units per second.
    #[must_use]
    pub fn avg_rate(&self) -> f64 {
        rate(self.count, self.elapsed)
    }
}

/// The phase currently in progress.
struct ActivePhase {
    phase: ProgressPhase,
    unit: ProgressUnit,
    total: Option<u64>,
    current: u64,
    started: Instant,
    last_sample_at: Instant,
    last_sample_count: u64,
    samples: Vec<f64>,
    peak_rate: f64,
}

impl ActivePhase {
    fn new(phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>, now: Instant) -> Self {
        Self {
            phase,
            unit,
            total,
            current: 0,
            started: now,
            last_sample_at: now,
            last_sample_count: 0,
            samples: Vec::new(),
            peak_rate: 0.0,
        }
    }

    /// Record a throughput sample once a sampling interval has passed (or, with
    /// `force`, for whatever partial interval remains).
    fn sample(&mut self, now: Instant, force: bool) {
        let dt = now.duration_since(self.last_sample_at);
        if dt < SAMPLE_EVERY && !(force && dt >= Duration::from_millis(20)) {
            return;
        }
        let r = rate(self.current.saturating_sub(self.last_sample_count), dt);
        self.samples.push(r);
        self.peak_rate = self.peak_rate.max(r);
        self.last_sample_at = now;
        self.last_sample_count = self.current;
    }

    fn into_record(mut self, now: Instant, origin: Instant) -> PhaseRecord {
        self.sample(now, true);
        PhaseRecord {
            phase: self.phase,
            unit: self.unit,
            count: self.current,
            started_at: self.started.duration_since(origin),
            elapsed: now.duration_since(self.started),
            samples: self.samples,
            peak_rate: self.peak_rate,
        }
    }
}

/// Progress sink for network transfers and checkout.
///
/// Draws only in human mode on a TTY; always records [`PhaseRecord`]s.
pub struct TerminalProgress {
    origin: Instant,
    draw: bool,
    color: bool,
    active: Option<ActivePhase>,
    finished: Vec<PhaseRecord>,
    last_draw: Instant,
    frame: usize,
    remote_lines: SidebandLines,
}

impl TerminalProgress {
    /// Create a sink; it draws nothing unless `mode` is human and stderr is a TTY.
    #[must_use]
    pub fn new(mode: OutputMode) -> Self {
        let draw = mode == OutputMode::Human && io::stderr().is_terminal();
        Self {
            origin: Instant::now(),
            draw,
            color: draw
                && grit_lib::porcelain::terminal::stderr_supports_color()
                && std::env::var_os("NO_COLOR").is_none(),
            active: None,
            finished: Vec::new(),
            last_draw: Instant::now() - MIN_REDRAW,
            frame: 0,
            remote_lines: SidebandLines::new(),
        }
    }

    /// Whether live progress is being drawn on stderr.
    #[must_use]
    pub fn is_drawing(&self) -> bool {
        self.draw
    }

    /// Whether drawn output uses ANSI color.
    #[must_use]
    pub fn uses_color(&self) -> bool {
        self.color
    }

    /// Phases finished so far, in order.
    #[must_use]
    pub fn phases(&self) -> &[PhaseRecord] {
        &self.finished
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
        let line = render_live(active, now, self.frame, terminal_width(), self.color);
        let mut err = io::stderr();
        let _ = write!(err, "\r{}{line}", clear_seq(self.color));
        let _ = err.flush();
        self.last_draw = now;
    }

    fn finish_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let record = active.into_record(Instant::now(), self.origin);
        // A phase that processed nothing (e.g. an empty local copy step) is noise.
        if record.count == 0 {
            return;
        }
        if self.draw {
            let line = render_done(&record, terminal_width(), self.color);
            let mut err = io::stderr();
            let _ = writeln!(err, "\r{}{line}", clear_seq(self.color));
            let _ = err.flush();
        }
        self.finished.push(record);
    }

    fn render_remote(&mut self, line: RemoteLine) {
        let (RemoteLine::Update(text) | RemoteLine::Line(text)) = &line;
        if text.is_empty() {
            return;
        }
        let shown = paint(self.color, DIM, &format!("  remote: {text}"));
        let mut err = io::stderr();
        let _ = write!(err, "\r{}", clear_seq(self.color));
        match line {
            RemoteLine::Update(_) => {
                let _ = write!(err, "{shown}");
            }
            RemoteLine::Line(_) => {
                let _ = writeln!(err, "{shown}");
            }
        }
        let _ = err.flush();
        // Put the live phase line back underneath the remote message.
        self.redraw(true);
    }
}

impl Progress for TerminalProgress {
    fn message(&mut self, bytes: &[u8]) {
        if !self.draw {
            return;
        }
        let mut lines = Vec::new();
        self.remote_lines.feed(bytes, |line| lines.push(line));
        for line in lines {
            self.render_remote(line);
        }
    }

    fn remote_sideband_progress(&self) -> bool {
        self.draw
    }
}

impl ProgressSink for TerminalProgress {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.start_with_unit(phase, phase.default_unit(), total);
    }

    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.finish_active();
        self.active = Some(ActivePhase::new(phase, unit, total, Instant::now()));
        self.redraw(true);
    }

    fn inc(&mut self, units: u64) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.current = active.current.saturating_add(units);
        active.sample(Instant::now(), false);
        self.redraw(false);
    }

    fn set(&mut self, current: u64) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.current = current;
        active.sample(Instant::now(), false);
        self.redraw(false);
    }

    fn finish(&mut self) {
        self.finish_active();
    }
}

fn terminal_width() -> usize {
    terminal_size()
        .map(|(Width(w), _)| w as usize)
        .unwrap_or(80)
        .max(20)
}

/// Return to column 0 and erase the row: an ANSI erase when color is on, else
/// overwrite with spaces.
fn clear_seq(color: bool) -> String {
    if color {
        "\x1b[2K".to_owned()
    } else {
        format!("{}\r", " ".repeat(terminal_width()))
    }
}

pub(crate) const DIM: &str = "2";
pub(crate) const BOLD: &str = "1";
const GREEN: &str = "32";

/// Wrap `text` in an SGR sequence when `color` is on.
pub(crate) fn paint(color: bool, sgr: &str, text: &str) -> String {
    if color {
        format!("\x1b[{sgr}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Five-step 256-color ramp (dark to bright) for a phase's graph.
pub(crate) fn phase_palette(phase: ProgressPhase) -> [u8; 5] {
    match phase {
        ProgressPhase::ReceivingObjects => [24, 31, 38, 45, 51],
        ProgressPhase::IndexingObjects => [97, 134, 171, 207, 213],
        ProgressPhase::ResolvingDeltas => [130, 172, 178, 214, 220],
        ProgressPhase::CopyingObjects => [23, 30, 37, 44, 87],
        ProgressPhase::CheckingOutFiles => [22, 28, 34, 40, 82],
        _ => [240, 244, 248, 252, 255],
    }
}

/// Short label for a phase.
pub(crate) fn phase_label(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::ReceivingObjects => "Receiving",
        ProgressPhase::IndexingObjects => "Indexing",
        ProgressPhase::ResolvingDeltas => "Resolving",
        ProgressPhase::CopyingObjects => "Copying",
        ProgressPhase::CheckingOutFiles => "Checkout",
        _ => "Working",
    }
}

/// What one unit of `unit` is called during `phase`.
fn unit_noun(phase: ProgressPhase, unit: ProgressUnit) -> &'static str {
    match unit {
        ProgressUnit::Bytes => "",
        ProgressUnit::Files => "files",
        ProgressUnit::Objects if phase == ProgressPhase::ResolvingDeltas => "deltas",
        ProgressUnit::Objects => "objects",
    }
}

fn rate(units: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    units as f64 / secs
}

/// Shrink `samples` to at most `width` points by averaging equal buckets.
fn resample(samples: &[f64], width: usize) -> Vec<f64> {
    if samples.len() <= width || width == 0 {
        return samples.to_vec();
    }
    (0..width)
        .map(|i| {
            let start = i * samples.len() / width;
            let end = ((i + 1) * samples.len() / width).max(start + 1);
            let bucket = &samples[start..end];
            bucket.iter().sum::<f64>() / bucket.len() as f64
        })
        .collect()
}

/// Stretch or shrink `samples` to exactly `width` points, so a short phase's
/// graph still spans the full width once it is done.
fn fit(samples: &[f64], width: usize) -> Vec<f64> {
    if samples.is_empty() || samples.len() >= width {
        return resample(samples, width);
    }
    (0..width)
        .map(|i| samples[i * samples.len() / width])
        .collect()
}

/// Render `samples` as a `width`-cell sparkline scaled to `scale_max`,
/// right-aligned (newest on the right) and padded with blanks on the left.
pub(crate) fn sparkline(
    samples: &[f64],
    width: usize,
    scale_max: f64,
    palette: [u8; 5],
    color: bool,
) -> String {
    let shown = &samples[samples.len().saturating_sub(width)..];
    let mut out = String::new();
    out.push_str(&" ".repeat(width - shown.len()));
    for &value in shown {
        let level = if scale_max > 0.0 && value.is_finite() {
            ((value / scale_max) * 7.0).round().clamp(0.0, 7.0) as usize
        } else {
            0
        };
        let ch = LEVELS[level];
        if color {
            let shade = palette[(level * palette.len()) / LEVELS.len()];
            out.push_str(&format!("\x1b[38;5;{shade}m{ch}"));
        } else {
            out.push(ch);
        }
    }
    if color && !shown.is_empty() {
        out.push_str("\x1b[0m");
    }
    out
}

/// `1234567` → `1,234,567`.
pub(crate) fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `98123.4` → `98.1k`.
fn compact_count(n: f64) -> String {
    if !n.is_finite() || n < 0.0 {
        return "0".to_owned();
    }
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

/// Human throughput for `unit`.
pub(crate) fn format_unit_rate(unit: ProgressUnit, per_sec: f64, phase: ProgressPhase) -> String {
    match unit {
        ProgressUnit::Bytes => format_rate(per_sec),
        _ => format!("{} {}/s", compact_count(per_sec), unit_noun(phase, unit)),
    }
}

/// `340ms` under a second, `12.3s`, or `1m 05s` past a minute.
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let whole = d.as_secs();
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// Count text for a phase line.
fn count_text(
    phase: ProgressPhase,
    unit: ProgressUnit,
    current: u64,
    total: Option<u64>,
) -> String {
    match (unit, total) {
        (ProgressUnit::Bytes, _) => format_bytes(current),
        (_, Some(total)) if total > 0 => {
            format!("{}/{}", group_digits(current), group_digits(total))
        }
        _ => format!("{} {}", group_digits(current), unit_noun(phase, unit)),
    }
}

/// A completion bar plus percentage, e.g. `━━━━━━──── 61%`.
fn completion_bar(current: u64, total: u64, palette: [u8; 5], color: bool) -> (String, usize) {
    let pct = (current.saturating_mul(100) / total.max(1)).min(100);
    let filled = (pct as usize * BAR_WIDTH) / 100;
    let done = "━".repeat(filled);
    let todo = "─".repeat(BAR_WIDTH - filled);
    let text = if color {
        format!(
            "\x1b[38;5;{}m{done}\x1b[0m{} {pct:>3}%",
            palette[4],
            paint(true, DIM, &todo)
        )
    } else {
        format!("{done}{todo} {pct:>3}%")
    };
    (text, BAR_WIDTH + 5)
}

/// Pick a graph width that fits `width` columns next to `fixed` columns of text.
fn graph_width(width: usize, fixed: usize) -> usize {
    let room = width.saturating_sub(fixed + 1);
    if room < GRAPH_MIN {
        0
    } else {
        room.min(GRAPH_MAX)
    }
}

/// Assemble `<mark> <label>  <graph>  <stats>` within `width` visible columns.
fn layout(
    mark: &str,
    phase: ProgressPhase,
    graph: impl Fn(usize) -> String,
    stats: &[(String, usize)],
    width: usize,
    color: bool,
) -> String {
    let label = format!("{:<LABEL_WIDTH$}", phase_label(phase));
    let stats_width: usize = stats.iter().map(|(_, w)| w + 2).sum();
    let fixed = 2 + LABEL_WIDTH + 2 + stats_width;
    let gw = graph_width(width, fixed);
    let mut line = format!("{mark} {}", paint(color, BOLD, &label));
    if gw > 0 {
        line.push_str("  ");
        line.push_str(&graph(gw));
    }
    let mut used = 2 + LABEL_WIDTH + if gw > 0 { 2 + gw } else { 0 };
    for (text, w) in stats {
        if used + 2 + w > width {
            break;
        }
        line.push_str("  ");
        line.push_str(text);
        used += 2 + w;
    }
    line
}

/// Right-align `text` in `width` cells so stats line up across phase lines.
fn column(text: String, width: usize) -> (String, usize) {
    let w = text.chars().count();
    if w >= width {
        return (text, w);
    }
    (format!("{}{text}", " ".repeat(width - w)), width)
}

/// Column widths for the count, rate and duration stats.
const COUNT_COL: usize = 16;
const RATE_COL: usize = 17;
const TIME_COL: usize = 6;

/// The in-place line for a running phase.
fn render_live(
    active: &ActivePhase,
    now: Instant,
    frame: usize,
    width: usize,
    color: bool,
) -> String {
    let palette = phase_palette(active.phase);
    let spin = SPINNER[frame % SPINNER.len()].to_string();
    let mark = if color {
        format!("\x1b[38;5;{}m{spin}\x1b[0m", palette[4])
    } else {
        spin
    };
    let elapsed = now.duration_since(active.started);
    let current_rate = active
        .samples
        .last()
        .copied()
        .unwrap_or_else(|| rate(active.current, elapsed));

    let mut stats = vec![column(
        count_text(active.phase, active.unit, active.current, active.total),
        COUNT_COL,
    )];
    if let Some(total) = active.total.filter(|&t| t > 0) {
        stats.push(completion_bar(active.current, total, palette, color));
    }
    stats.push(column(
        format_unit_rate(active.unit, current_rate, active.phase),
        RATE_COL,
    ));
    let (elapsed_text, elapsed_w) = column(format_duration(elapsed), TIME_COL);
    stats.push((paint(color, DIM, &elapsed_text), elapsed_w));

    let peak = active.peak_rate;
    layout(
        &mark,
        active.phase,
        |gw| sparkline(&active.samples, gw, peak, palette, color),
        &stats,
        width,
        color,
    )
}

/// The persistent line for a finished phase: the whole phase's graph, totals,
/// average throughput and duration.
fn render_done(record: &PhaseRecord, width: usize, color: bool) -> String {
    let palette = phase_palette(record.phase);
    let mark = paint(color, GREEN, "✓");
    let stats = vec![
        column(
            count_text(record.phase, record.unit, record.count, None),
            COUNT_COL,
        ),
        column(
            format_unit_rate(record.unit, record.avg_rate(), record.phase),
            RATE_COL,
        ),
        {
            let (t, w) = column(format_duration(record.elapsed), TIME_COL);
            (paint(color, DIM, &t), w)
        },
    ];
    layout(
        &mark,
        record.phase,
        |gw| {
            let points = fit(&record.samples, gw);
            let max = points.iter().copied().fold(0.0_f64, f64::max);
            sparkline(&points, gw, max, palette, color)
        },
        &stats,
        width,
        color,
    )
}

/// Format a byte count for display (binary units).
#[must_use]
pub fn format_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n == 0 {
        return "0 B".to_owned();
    }
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= KIB && unit + 1 < UNITS.len() {
        value /= KIB;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Format a transfer rate (bytes per second).
#[must_use]
pub fn format_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "0 B/s".to_owned();
    }
    format!("{}/s", format_bytes(bytes_per_sec.round() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Visible width of a rendered line (ANSI escapes stripped).
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

    fn record(
        phase: ProgressPhase,
        unit: ProgressUnit,
        count: u64,
        samples: Vec<f64>,
    ) -> PhaseRecord {
        let peak_rate = samples.iter().copied().fold(0.0, f64::max);
        PhaseRecord {
            phase,
            unit,
            count,
            started_at: Duration::ZERO,
            elapsed: Duration::from_secs(4),
            samples,
            peak_rate,
        }
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
    }

    #[test]
    fn formats_rate() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(1024.0), "1.00 KiB/s");
    }

    #[test]
    fn groups_digits_and_compacts_counts() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_000_323), "1,000,323");
        assert_eq!(compact_count(950.0), "950");
        assert_eq!(compact_count(98_123.0), "98.1k");
        assert_eq!(compact_count(2_500_000.0), "2.5M");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(2_340)), "2.3s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn sparkline_scales_to_max_and_right_aligns() {
        let line = sparkline(&[0.0, 5.0, 10.0], 5, 10.0, [1, 2, 3, 4, 5], false);
        assert_eq!(line, "  ▁▅█");
        assert_eq!(sparkline(&[], 3, 0.0, [1, 2, 3, 4, 5], false), "   ");
    }

    #[test]
    fn sparkline_keeps_newest_samples_when_scrolling() {
        let samples: Vec<f64> = (0..20).map(f64::from).collect();
        let line = sparkline(&samples, 4, 19.0, [1, 2, 3, 4, 5], false);
        assert_eq!(line.chars().count(), 4);
        assert!(line.ends_with('█'), "{line}");
    }

    #[test]
    fn colored_sparkline_has_same_visible_cells() {
        let line = sparkline(
            &[1.0, 2.0, 3.0],
            6,
            3.0,
            phase_palette(ProgressPhase::ReceivingObjects),
            true,
        );
        assert!(line.contains("\x1b[38;5;"));
        assert_eq!(visible(&line).chars().count(), 6);
    }

    #[test]
    fn fit_stretches_short_histories_to_full_width() {
        assert_eq!(fit(&[1.0, 2.0], 4), vec![1.0, 1.0, 2.0, 2.0]);
        assert_eq!(fit(&[1.0, 3.0, 5.0, 7.0], 2), vec![2.0, 6.0]);
        assert!(fit(&[], 4).is_empty());
    }

    #[test]
    fn resample_averages_buckets() {
        assert_eq!(resample(&[1.0, 3.0, 5.0, 7.0], 2), vec![2.0, 6.0]);
        assert_eq!(resample(&[1.0, 2.0], 8), vec![1.0, 2.0]);
    }

    #[test]
    fn done_line_shows_totals_rate_and_time() {
        let rec = record(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            8 * 1024 * 1024,
            vec![1.0, 4.0, 2.0, 8.0],
        );
        let line = visible(&render_done(&rec, 100, true));
        assert!(line.starts_with("✓ Receiving"), "{line}");
        assert!(line.contains("8.00 MiB"), "{line}");
        assert!(line.contains("2.00 MiB/s"), "{line}");
        assert!(line.contains("4.0s"), "{line}");
        assert!(line.contains('█'), "graph missing: {line}");
    }

    #[test]
    fn object_phases_use_object_units_not_bytes() {
        let rec = record(
            ProgressPhase::IndexingObjects,
            ProgressUnit::Objects,
            1_000_323,
            vec![200_000.0, 250_000.0],
        );
        let line = visible(&render_done(&rec, 100, false));
        assert!(line.contains("1,000,323 objects"), "{line}");
        assert!(line.contains("250.1k objects/s"), "{line}");
        assert!(!line.contains("KiB"), "{line}");

        let deltas = record(
            ProgressPhase::ResolvingDeltas,
            ProgressUnit::Objects,
            10,
            vec![2.5],
        );
        assert!(visible(&render_done(&deltas, 100, false)).contains("10 deltas"));
    }

    #[test]
    fn live_line_shows_bar_for_known_totals() {
        let now = Instant::now();
        let mut active = ActivePhase::new(
            ProgressPhase::IndexingObjects,
            ProgressUnit::Objects,
            Some(200),
            now - Duration::from_secs(2),
        );
        active.current = 50;
        active.samples = vec![10.0, 20.0, 25.0];
        active.peak_rate = 25.0;
        let line = visible(&render_live(&active, now, 0, 120, true));
        assert!(line.contains("Indexing"), "{line}");
        assert!(line.contains("50/200"), "{line}");
        assert!(line.contains(" 25%"), "{line}");
        assert!(line.contains("25 objects/s"), "{line}");
    }

    #[test]
    fn lines_fit_narrow_terminals() {
        let rec = record(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            300 * 1024 * 1024,
            vec![1.0; 50],
        );
        for width in [20_usize, 40, 60, 80, 200] {
            let line = visible(&render_done(&rec, width, true));
            assert!(line.chars().count() <= width, "width {width}: {line:?}");
        }
        // Wide terminals cap the graph instead of stretching it.
        let wide = visible(&render_done(&rec, 300, false));
        assert!(wide.chars().filter(|c| LEVELS.contains(c)).count() <= GRAPH_MAX);
    }

    #[test]
    fn sink_records_phases_without_drawing() {
        let mut sink = TerminalProgress::new(OutputMode::Json);
        assert!(!sink.is_drawing());
        sink.start_with_unit(ProgressPhase::ReceivingObjects, ProgressUnit::Bytes, None);
        sink.inc(1024);
        sink.inc(1024);
        // Starting the next phase closes the previous one.
        sink.start(ProgressPhase::IndexingObjects, Some(3));
        sink.set(3);
        sink.finish();
        let phases = sink.phases();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].phase, ProgressPhase::ReceivingObjects);
        assert_eq!(phases[0].unit, ProgressUnit::Bytes);
        assert_eq!(phases[0].count, 2048);
        assert_eq!(phases[1].unit, ProgressUnit::Objects);
        assert_eq!(phases[1].count, 3);
        sink.inc(5);
        assert_eq!(
            sink.phases().len(),
            2,
            "updates outside a phase are ignored"
        );
        sink.start(ProgressPhase::CopyingObjects, Some(0));
        sink.finish();
        assert_eq!(sink.phases().len(), 2, "empty phases are not recorded");
    }
}

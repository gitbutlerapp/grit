//! End-of-transfer overview shared by clone, fetch and pull: a time bar that
//! splits the run across its phases, then what was received and how fast.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use grit_lib::objects::index_pack::ObjectKindCounts;
use grit_lib::progress::{ProgressPhase, ProgressUnit};
use grit_lib::remote::fetch::FetchMilestone;
use grit_lib::remote::transfer::TransferStats;
use schemars::JsonSchema;
use serde::Serialize;

use super::{
    format_bytes, format_duration, format_rate, group_digits, paint, phase_label, phase_palette,
    rate, MilestoneRecord, PhaseRecord, BOLD, DIM,
};
use crate::output::outln;

/// Glyphs for the time bar when color is off, so phases stay distinguishable.
const PLAIN_SEGMENTS: [char; 4] = ['█', '▓', '▒', '░'];
pub(crate) const TIME_BAR_WIDTH: usize = 40;
/// Shortest receiving time worth reporting a transfer rate for.
const MIN_TIMED_RECEIVE: Duration = Duration::from_millis(10);

/// Objects received, by type.
#[derive(Serialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectTypes {
    pub commits: u64,
    pub trees: u64,
    pub blobs: u64,
    /// Annotated tag objects (not refs).
    pub tags: u64,
}

impl From<ObjectKindCounts> for ObjectTypes {
    fn from(k: ObjectKindCounts) -> Self {
        Self {
            commits: k.commits,
            trees: k.trees,
            blobs: k.blobs,
            tags: k.tags,
        }
    }
}

/// What a transfer received and where its time went, for the human summary.
#[derive(Clone, Debug, Default)]
pub struct TransferSummary {
    /// Wall-clock time of the whole command, measured from the progress sink's start.
    pub elapsed: Duration,
    /// Finished progress phases, in order.
    pub records: Vec<PhaseRecord>,
    /// Wire fetch milestones, in order; they split the wait before the pack.
    pub milestones: Vec<MilestoneRecord>,
    /// Objects received.
    pub objects: u64,
    /// Pack bytes received.
    pub bytes: u64,
    /// Objects received by type, when the pack was indexed locally.
    pub object_types: Option<ObjectTypes>,
    /// When true, the data row says "sent" instead of "received" (push).
    pub transfer_sent: bool,
}

impl TransferSummary {
    /// Summary of `stats`, timed by `records` over `elapsed`.
    #[must_use]
    pub fn new(stats: TransferStats, records: Vec<PhaseRecord>, elapsed: Duration) -> Self {
        Self {
            elapsed,
            records,
            milestones: Vec::new(),
            objects: stats.objects,
            bytes: stats.bytes,
            object_types: stats.kinds.map(ObjectTypes::from),
            transfer_sent: false,
        }
    }

    /// Also split the time bar at these wire fetch milestones.
    #[must_use]
    pub fn with_milestones(mut self, milestones: Vec<MilestoneRecord>) -> Self {
        self.milestones = milestones;
        self
    }

    /// Whether anything was received (an up-to-date fetch has nothing to show).
    #[must_use]
    pub fn received_anything(&self) -> bool {
        self.objects > 0 || self.bytes > 0
    }

    /// Write the `time`, `data` and `objects` rows, skipping rows with nothing to say.
    pub fn render_rows(&self, w: &mut dyn Write, color: bool) -> Result<()> {
        let total = format_duration(self.elapsed);
        if !self.records.is_empty() {
            let (bar, legend) = time_bar(&self.records, &self.milestones, self.elapsed, color);
            outln!(w, "  {}  {bar}  {total}", row_label(color, "time"));
            outln!(w, "  {}  {legend}", row_label(color, ""));
        }

        // Report bytes off the wire, as the live Receiving lines did. The stored
        // pack can be larger: a thin fetch pack gets its missing delta bases
        // appended. Local copies have no Receiving phase, so use the pack size.
        let wire_phases = if self.transfer_sent {
            [ProgressPhase::SendingObjects]
        } else {
            [ProgressPhase::ReceivingObjects]
        };
        let wire: Vec<&PhaseRecord> = self
            .records
            .iter()
            .filter(|r| {
                wire_phases.contains(&r.phase) && r.unit == ProgressUnit::Bytes && r.count > 0
            })
            .collect();
        let wire_bytes: u64 = wire.iter().map(|r| r.count).sum();
        let payload = if wire_bytes > 0 {
            wire_bytes
        } else {
            self.bytes
        };
        if payload > 0 {
            let verb = if self.transfer_sent {
                "sent"
            } else {
                "received"
            };
            let mut data = vec![format!("{} {verb}", format_bytes(payload))];
            let busy: Duration = wire.iter().map(|r| r.elapsed).sum();
            // A small pack can land in one read; its rate would be meaningless.
            if busy >= MIN_TIMED_RECEIVE {
                let avg = rate(wire_bytes, busy);
                let peak = wire.iter().map(|r| r.peak_rate).fold(0.0, f64::max);
                data.push(format!("{} avg", format_rate(avg)));
                // Sampling can miss the burst entirely; a peak under the average says nothing.
                if peak >= avg {
                    data.push(format!("{} peak", format_rate(peak)));
                }
            }
            outln!(w, "  {}  {}", row_label(color, "data"), join(&data, color));
        }

        if self.objects > 0 {
            let mut objects = vec![group_digits(self.objects)];
            if let Some(t) = self.object_types {
                for (n, one, many) in [
                    (t.commits, "commit", "commits"),
                    (t.trees, "tree", "trees"),
                    (t.blobs, "blob", "blobs"),
                    (t.tags, "tag", "tags"),
                ] {
                    if n > 0 {
                        objects.push(plural(n, one, many));
                    }
                }
            }
            outln!(
                w,
                "  {}  {}",
                row_label(color, "objects"),
                join(&objects, color)
            );
        }
        Ok(())
    }
}

/// The line printed on stderr before a fetch or pull starts drawing progress:
/// `<verb> <remote> <url>`, leaving out the URL when it is the remote argument
/// itself (a literal URL or path rather than a configured remote name).
#[must_use]
pub fn transfer_header(verb: &str, remote: &str, url: Option<&str>, color: bool) -> String {
    let mut line = format!(
        "{} {}",
        paint(color, BOLD, verb),
        paint(color, BOLD, remote)
    );
    if let Some(url) = url.filter(|u| !u.is_empty() && *u != remote) {
        line.push(' ');
        line.push_str(&paint(color, DIM, url));
    }
    line
}

/// Whether the human summary on stdout should use ANSI color.
#[must_use]
pub fn stdout_color() -> bool {
    grit_lib::porcelain::terminal::stdout_supports_color() && std::env::var_os("NO_COLOR").is_none()
}

/// A dimmed, padded label for one summary row.
#[must_use]
pub fn row_label(color: bool, text: &str) -> String {
    paint(color, DIM, &format!("{text:<8}"))
}

/// `n` with digit grouping and the singular or plural noun.
#[must_use]
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// Join row parts with a dimmed middle dot.
#[must_use]
pub fn join(parts: &[String], color: bool) -> String {
    parts.join(&paint(color, DIM, " · "))
}

/// One stretch of the timeline: a progress phase, or a gap between them.
pub(crate) struct Segment {
    pub(crate) name: &'static str,
    duration: Duration,
    phase: Option<ProgressPhase>,
}

/// Lay the phases out on the command's timeline, naming the gaps: before the
/// first byte arrives (connecting, ref negotiation, the server building the
/// pack), between phases, and after the last one (writing refs and config).
///
/// Wire fetch `milestones` split a gap where they fall: the stretch up to
/// [`FetchMilestone::Connected`] is the handshake, from there to
/// [`FetchMilestone::PackRequested`] is ref listing and negotiation, and from
/// there to the next phase is the server preparing the pack. Without them the
/// first gap is just "connect".
pub(crate) fn timeline(
    records: &[PhaseRecord],
    milestones: &[MilestoneRecord],
    total: Duration,
) -> Vec<Segment> {
    const MIN_GAP: Duration = Duration::from_millis(5);
    let mut segments = Vec::new();
    let mut cursor = Duration::ZERO;
    let mut marks = milestones.iter().peekable();
    let push_gap =
        |segments: &mut Vec<Segment>, from: Duration, to: Duration, name: &'static str| {
            let gap = to.saturating_sub(from);
            if gap > MIN_GAP {
                segments.push(Segment {
                    name,
                    duration: gap,
                    phase: None,
                });
            }
        };
    for rec in records {
        // The milestone that opened the current stretch of the gap, if any.
        let mut opened_by = None;
        while let Some(mark) = marks.next_if(|m| m.at <= rec.started_at) {
            let at_start = segments.is_empty() && opened_by.is_none();
            let name = gap_name(opened_by, Some(mark.milestone), at_start);
            push_gap(&mut segments, cursor, mark.at, name);
            cursor = cursor.max(mark.at);
            opened_by = Some(mark.milestone);
        }
        let at_start = segments.is_empty() && opened_by.is_none();
        let name = gap_name(opened_by, None, at_start);
        push_gap(&mut segments, cursor, rec.started_at, name);
        segments.push(Segment {
            name: phase_label(rec.phase),
            duration: rec.elapsed,
            phase: Some(rec.phase),
        });
        cursor = rec.started_at + rec.elapsed;
    }
    push_gap(&mut segments, cursor, total, "finish");
    segments
}

/// Gap names that come from wire fetch milestones (see [`gap_name`]).
const MILESTONE_GAPS: [&str; 3] = ["handshake", "negotiate", "pack prep"];

/// Name for a stretch of waiting between `opened_by` and `closed_by`;
/// `at_start` when nothing has happened before it.
fn gap_name(
    opened_by: Option<FetchMilestone>,
    closed_by: Option<FetchMilestone>,
    at_start: bool,
) -> &'static str {
    match (opened_by, closed_by) {
        (Some(FetchMilestone::Connected), _) => "negotiate",
        (Some(FetchMilestone::PackRequested), _) => "pack prep",
        (None, Some(FetchMilestone::Connected)) => "handshake",
        _ if at_start => "connect",
        _ => "other",
    }
}

/// A bar splitting the total time across the timeline, plus its legend.
pub(crate) fn time_bar(
    records: &[PhaseRecord],
    milestones: &[MilestoneRecord],
    total: Duration,
    color: bool,
) -> (String, String) {
    const GAP_SHADE: u8 = 240;
    let segments = timeline(records, milestones, total);
    let total_secs = total.as_secs_f64().max(f64::EPSILON);
    let mut bar = String::new();
    let mut legend = Vec::new();
    let mut used = 0usize;
    let mut phase_index = 0usize;
    for seg in &segments {
        let share = seg.duration.as_secs_f64() / total_secs;
        let cells = ((share * TIME_BAR_WIDTH as f64).round() as usize)
            .max(usize::from(share > 0.0))
            .min(TIME_BAR_WIDTH - used);
        let (glyph, shade) = match seg.phase {
            Some(phase) => {
                let glyph = PLAIN_SEGMENTS[phase_index % PLAIN_SEGMENTS.len()];
                phase_index += 1;
                (glyph, phase_palette(phase)[3])
            }
            // Network waits get muted hues of their own so the stretch
            // before the pack reads as three parts; other gaps stay grey.
            None => match seg.name {
                "handshake" => ('·', 60),
                "negotiate" => ('·', 66),
                "pack prep" => ('·', 96),
                _ => ('·', GAP_SHADE),
            },
        };
        let (cell, marker) = if color {
            ('█', '■')
        } else {
            (glyph, glyph)
        };
        let run: String = std::iter::repeat_n(cell, cells).collect();
        bar.push_str(&if color {
            format!("\x1b[38;5;{shade}m{run}\x1b[0m")
        } else {
            run
        });
        used += cells;
        // Small gaps stay in the bar but out of the legend, except the
        // network stretches a fetch milestone named, which are always listed.
        if seg.phase.is_none() && share < 0.03 && !MILESTONE_GAPS.contains(&seg.name) {
            continue;
        }
        let name = seg.name.to_lowercase();
        let dur = format_duration(seg.duration);
        legend.push(if color {
            format!("\x1b[38;5;{shade}m{marker}\x1b[0m {name} {dur}")
        } else {
            format!("{marker} {name} {dur}")
        });
    }
    if used < TIME_BAR_WIDTH {
        bar.push_str(&" ".repeat(TIME_BAR_WIDTH - used));
    }
    (bar, legend.join("  "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        phase: ProgressPhase,
        unit: ProgressUnit,
        start_ms: u64,
        ms: u64,
        count: u64,
    ) -> PhaseRecord {
        PhaseRecord {
            phase,
            unit,
            count,
            started_at: Duration::from_millis(start_ms),
            elapsed: Duration::from_millis(ms),
            samples: vec![1.0, 2.0],
            peak_rate: 4.0 * 1024.0 * 1024.0,
        }
    }

    #[test]
    fn timeline_names_gaps_between_phases() {
        let records = vec![
            rec(
                ProgressPhase::ReceivingObjects,
                ProgressUnit::Bytes,
                500,
                1000,
                10,
            ),
            rec(
                ProgressPhase::IndexingObjects,
                ProgressUnit::Objects,
                1500,
                200,
                10,
            ),
            rec(
                ProgressPhase::CheckingOutFiles,
                ProgressUnit::Files,
                1900,
                50,
                10,
            ),
        ];
        let names: Vec<&str> = timeline(&records, &[], Duration::from_secs(2))
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "connect",
                "Receiving",
                "Indexing",
                "other",
                "Checkout",
                "finish"
            ]
        );
    }

    #[test]
    fn milestones_split_the_wait_before_the_pack() {
        let records = vec![rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            1800,
            400,
            10,
        )];
        let mark = |milestone, ms| MilestoneRecord {
            milestone,
            at: Duration::from_millis(ms),
        };
        let milestones = [
            mark(FetchMilestone::Connected, 1100),
            mark(FetchMilestone::PackRequested, 1400),
        ];
        let segments = timeline(&records, &milestones, Duration::from_millis(2200));
        let got: Vec<(&str, u128)> = segments
            .iter()
            .map(|s| (s.name, s.duration.as_millis()))
            .collect();
        assert_eq!(
            got,
            [
                ("handshake", 1100),
                ("negotiate", 300),
                ("pack prep", 400),
                ("Receiving", 400)
            ]
        );
        let (_, legend) = time_bar(&records, &milestones, Duration::from_millis(2200), false);
        for part in ["handshake 1.1s", "negotiate 300ms", "pack prep 400ms"] {
            assert!(legend.contains(part), "{legend}");
        }
    }

    #[test]
    fn time_bar_fills_its_width_and_lists_phases() {
        let records = vec![
            rec(
                ProgressPhase::ReceivingObjects,
                ProgressUnit::Bytes,
                0,
                1500,
                10,
            ),
            rec(
                ProgressPhase::CheckingOutFiles,
                ProgressUnit::Files,
                1500,
                500,
                10,
            ),
        ];
        let (bar, legend) = time_bar(&records, &[], Duration::from_secs(2), false);
        assert_eq!(bar.chars().count(), TIME_BAR_WIDTH);
        assert_eq!(bar.chars().filter(|&c| c == '█').count(), 30);
        assert!(legend.contains("receiving 1.5s"), "{legend}");
        assert!(legend.contains("checkout 500ms"), "{legend}");
    }

    #[test]
    fn summary_from_stats_keeps_counts_and_types() {
        let stats = TransferStats {
            objects: 7,
            bytes: 2048,
            kinds: Some(ObjectKindCounts {
                commits: 2,
                trees: 2,
                blobs: 3,
                tags: 0,
            }),
        };
        let summary = TransferSummary::new(stats, Vec::new(), Duration::from_secs(1));
        assert!(summary.received_anything());
        assert_eq!(summary.objects, 7);
        assert_eq!(summary.bytes, 2048);
        assert_eq!(
            summary.object_types,
            Some(ObjectTypes {
                commits: 2,
                trees: 2,
                blobs: 3,
                tags: 0
            })
        );
        assert!(!TransferSummary::default().received_anything());
    }

    #[test]
    fn rows_report_time_data_and_objects_without_color() {
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_secs(2),
            records: vec![rec(
                ProgressPhase::ReceivingObjects,
                ProgressUnit::Bytes,
                0,
                1000,
                2 * 1024 * 1024,
            )],
            objects: 1_234,
            bytes: 2 * 1024 * 1024,
            object_types: Some(ObjectTypes {
                commits: 100,
                trees: 400,
                blobs: 734,
                tags: 0,
            }),
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows.len(), 4, "{text}");
        assert!(rows[0].starts_with("  time      "), "{text}");
        assert!(rows[0].ends_with("  2.0s"), "{text}");
        assert!(
            rows[2].contains("2.00 MiB received · 2.00 MiB/s avg · 4.00 MiB/s peak"),
            "{text}"
        );
        assert!(
            rows[3].contains("1,234 · 100 commits · 400 trees · 734 blobs"),
            "{text}"
        );
        assert!(
            !text.contains("0 tags"),
            "zero type counts are omitted: {text}"
        );
        assert!(!text.contains('\x1b'), "no escapes without color: {text}");
    }

    #[test]
    fn data_row_reports_wire_bytes_not_the_completed_thin_pack() {
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_secs(2),
            records: vec![rec(
                ProgressPhase::ReceivingObjects,
                ProgressUnit::Bytes,
                0,
                1000,
                1024 * 1024,
            )],
            objects: 10,
            // The stored pack grew when its delta bases were appended.
            bytes: 3 * 1024 * 1024,
            object_types: None,
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("1.00 MiB received · 1.00 MiB/s avg"),
            "{text}"
        );
        assert!(!text.contains("3.00 MiB"), "{text}");
    }

    #[test]
    fn data_row_sums_receiving_phases_across_remotes() {
        let mut second = rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            1500,
            1000,
            3 * 1024 * 1024,
        );
        second.peak_rate = 8.0 * 1024.0 * 1024.0;
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_secs(3),
            records: vec![
                rec(
                    ProgressPhase::ReceivingObjects,
                    ProgressUnit::Bytes,
                    0,
                    1000,
                    1024 * 1024,
                ),
                second,
            ],
            objects: 20,
            bytes: 4 * 1024 * 1024,
            object_types: None,
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("4.00 MiB received · 2.00 MiB/s avg · 8.00 MiB/s peak"),
            "{text}"
        );
    }

    #[test]
    fn data_row_omits_rates_for_an_untimed_burst() {
        let mut burst = rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            0,
            0,
            201 * 1024,
        );
        burst.peak_rate = 0.0;
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_secs(1),
            records: vec![burst],
            objects: 249,
            bytes: 201 * 1024,
            object_types: None,
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("  data      201 KiB received\n"), "{text}");
    }

    #[test]
    fn data_row_drops_a_peak_below_the_average() {
        let mut recv = rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            0,
            1000,
            2 * 1024 * 1024,
        );
        recv.peak_rate = 1024.0;
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_secs(2),
            records: vec![recv],
            objects: 5,
            bytes: 2 * 1024 * 1024,
            object_types: None,
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("2.00 MiB/s avg\n"), "{text}");
        assert!(!text.contains("peak"), "{text}");
    }

    #[test]
    fn data_row_falls_back_to_pack_size_for_local_copies() {
        let summary = TransferSummary {
            milestones: Vec::new(),
            elapsed: Duration::from_millis(10),
            records: vec![rec(
                ProgressPhase::CopyingObjects,
                ProgressUnit::Objects,
                0,
                5,
                3,
            )],
            objects: 3,
            bytes: 293,
            object_types: None,
            transfer_sent: false,
        };
        let mut out = Vec::new();
        summary.render_rows(&mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("  data      293 B received\n"), "{text}");
    }

    #[test]
    fn header_names_the_remote_and_its_url_once() {
        assert_eq!(
            transfer_header(
                "Fetching",
                "origin",
                Some("https://example.com/r.git"),
                false
            ),
            "Fetching origin https://example.com/r.git"
        );
        assert_eq!(
            transfer_header("Fetching", "../r.git", Some("../r.git"), false),
            "Fetching ../r.git"
        );
        assert_eq!(
            transfer_header("Pulling", "origin", None, false),
            "Pulling origin"
        );
    }

    #[test]
    fn rows_are_empty_when_nothing_was_received() {
        let mut out = Vec::new();
        TransferSummary::default()
            .render_rows(&mut out, false)
            .expect("render");
        assert!(out.is_empty());
    }
}

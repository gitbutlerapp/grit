//! `grit clone` — copy a remote repository into a new directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::remote::clone::{
    cleanup_failed_clone, clone as clone_repo, clone_effective_single_branch, derive_clone_dir,
    dest_preexisted_empty, CloneCheckout, CloneError, CloneOptions,
    CloneOutcome as LibCloneOutcome, CloneTags,
};
use grit_lib::remote::DefaultHttpClientFactory;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::output::{outln, progress, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::{
    format_bytes, format_duration, format_rate, group_digits, paint, phase_label, phase_palette,
    PhaseRecord, TerminalProgress, BOLD, DIM,
};
use grit_lib::progress::{ProgressPhase, ProgressUnit};

/// Objects received, by type.
#[derive(Serialize, JsonSchema, Clone, Copy)]
pub struct ObjectTypes {
    pub commits: u64,
    pub trees: u64,
    pub blobs: u64,
    /// Annotated tag objects (not refs).
    pub tags: u64,
}

/// Timing for one step of the clone.
#[derive(Serialize, JsonSchema)]
pub struct ClonePhase {
    /// `receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`.
    pub phase: String,
    /// Unit of `count`: `bytes`, `objects` or `files`.
    pub unit: String,
    /// Units processed in this step.
    pub count: u64,
    /// Wall-clock seconds spent in this step.
    pub seconds: f64,
}

/// Options for [`run`].
pub struct CloneRunOptions {
    pub url: String,
    pub dir: Option<String>,
    pub shallow: bool,
    pub single_branch: Option<bool>,
    pub no_tags: bool,
    pub origin: Option<String>,
    pub mode: OutputMode,
}

/// Result of `grit clone`.
#[derive(Serialize, JsonSchema)]
pub struct CloneOutcome {
    pub url: String,
    /// Destination directory.
    pub path: String,
    /// Remote name configured for the source (`origin` by default).
    pub remote: String,
    /// Whether only one branch was fetched (`git clone --single-branch` semantics).
    pub single_branch: bool,
    /// Tag fetch mode used for the initial fetch: `all`, `following`, or `none`.
    pub tag_fetch: String,
    /// Default branch checked out.
    pub branch: String,
    /// Objects received during the fetch.
    pub objects: u64,
    /// Pack bytes received during the fetch.
    pub bytes: u64,
    /// Remote branches fetched.
    pub branches: u64,
    /// Remote tags fetched.
    pub tags: u64,
    /// Working-tree files checked out.
    pub files: u64,
    /// Objects received by type, or `null` when the pack was copied from a
    /// local repository without being re-indexed.
    pub object_types: Option<ObjectTypes>,
    /// Total size of the checked-out files, in bytes.
    pub worktree_bytes: u64,
    /// Wall-clock seconds for the whole clone.
    pub seconds: f64,
    /// Time spent in each step, in order.
    pub phases: Vec<ClonePhase>,
    /// Why no working-tree checkout happened, when `files` is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_checkout_reason: Option<String>,
    /// Remote branch names that have commits when `no_checkout_reason` is `unborn_default_branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches_with_commits: Option<Vec<String>>,
    #[serde(skip)]
    #[schemars(skip)]
    pub elapsed: Duration,
    #[serde(skip)]
    #[schemars(skip)]
    records: Vec<PhaseRecord>,
}

impl HumanRender for CloneOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = stdout_color();
        render_summary(self, w, color)
    }
}

/// Whether the human summary on stdout should use ANSI color.
fn stdout_color() -> bool {
    grit_lib::porcelain::terminal::stdout_supports_color() && std::env::var_os("NO_COLOR").is_none()
}

/// Glyphs for the time bar when color is off, so phases stay distinguishable.
const PLAIN_SEGMENTS: [char; 4] = ['█', '▓', '▒', '░'];
const TIME_BAR_WIDTH: usize = 40;

/// The end-of-clone overview: where it went, how long each step took, and
/// what was transferred.
fn render_summary(o: &CloneOutcome, w: &mut dyn Write, color: bool) -> Result<()> {
    let label = |text: &str| paint(color, DIM, &format!("{text:<8}"));
    let total = format_duration(o.elapsed);
    if color {
        // Separate the summary from the progress lines above it on a terminal.
        outln!(w);
    }
    outln!(
        w,
        "{} Cloned into {} on {} in {}",
        paint(color, "32", "✓"),
        paint(color, BOLD, &o.path),
        paint(color, BOLD, &o.branch),
        paint(color, BOLD, &total),
    );
    outln!(w);

    if !o.records.is_empty() {
        let (bar, legend) = time_bar(&o.records, o.elapsed, color);
        outln!(w, "  {}  {bar}  {total}", label("time"));
        outln!(w, "  {}  {legend}", label(""));
    }

    if o.bytes > 0 {
        let mut data = vec![format!("{} received", format_bytes(o.bytes))];
        if let Some(recv) = o
            .records
            .iter()
            .find(|r| r.unit == ProgressUnit::Bytes && r.count > 0)
        {
            data.push(format!("{} avg", format_rate(recv.avg_rate())));
            data.push(format!("{} peak", format_rate(recv.peak_rate)));
        }
        outln!(w, "  {}  {}", label("data"), join(&data, color));
    }

    if o.objects > 0 {
        let mut objects = vec![group_digits(o.objects)];
        if let Some(t) = o.object_types {
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
        outln!(w, "  {}  {}", label("objects"), join(&objects, color));
    }

    outln!(
        w,
        "  {}  {}",
        label("refs"),
        join(
            &[
                plural(o.branches, "branch", "branches"),
                plural(o.tags, "tag", "tags"),
            ],
            color
        )
    );

    if o.files > 0 {
        outln!(
            w,
            "  {}  {}",
            label("files"),
            join(
                &[
                    plural(o.files, "file", "files"),
                    format!("{} in the work tree", format_bytes(o.worktree_bytes)),
                ],
                color
            )
        );
    }
    Ok(())
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

fn join(parts: &[String], color: bool) -> String {
    parts.join(&paint(color, DIM, " · "))
}

/// One stretch of the clone's timeline: a progress phase, or a gap between them.
struct Segment {
    name: &'static str,
    duration: Duration,
    phase: Option<ProgressPhase>,
}

/// Lay the phases out on the clone's timeline, naming the gaps: before the
/// first byte arrives (connecting, ref negotiation, the server building the
/// pack), between phases, and after the last one (writing refs and config).
fn timeline(records: &[PhaseRecord], total: Duration) -> Vec<Segment> {
    const MIN_GAP: Duration = Duration::from_millis(5);
    let mut segments = Vec::new();
    let mut cursor = Duration::ZERO;
    for rec in records {
        let gap = rec.started_at.saturating_sub(cursor);
        if gap > MIN_GAP {
            segments.push(Segment {
                name: if segments.is_empty() {
                    "connect"
                } else {
                    "other"
                },
                duration: gap,
                phase: None,
            });
        }
        segments.push(Segment {
            name: phase_label(rec.phase),
            duration: rec.elapsed,
            phase: Some(rec.phase),
        });
        cursor = rec.started_at + rec.elapsed;
    }
    let tail = total.saturating_sub(cursor);
    if tail > MIN_GAP {
        segments.push(Segment {
            name: "finish",
            duration: tail,
            phase: None,
        });
    }
    segments
}

/// A bar splitting the total time across the timeline, plus its legend.
fn time_bar(records: &[PhaseRecord], total: Duration, color: bool) -> (String, String) {
    const GAP_SHADE: u8 = 240;
    let segments = timeline(records, total);
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
            None => ('·', GAP_SHADE),
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
        // Small gaps stay in the bar but out of the legend.
        if seg.phase.is_none() && share < 0.03 {
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

fn phase_json_name(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::ReceivingObjects => "receiving",
        ProgressPhase::IndexingObjects => "indexing",
        ProgressPhase::ResolvingDeltas => "resolving_deltas",
        ProgressPhase::CopyingObjects => "copying",
        ProgressPhase::CheckingOutFiles => "checkout",
        _ => "other",
    }
}

fn unit_json_name(unit: ProgressUnit) -> &'static str {
    match unit {
        ProgressUnit::Bytes => "bytes",
        ProgressUnit::Objects => "objects",
        ProgressUnit::Files => "files",
    }
}

impl MarkdownRender for CloneOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let size = format_bytes(self.bytes);
        let secs = self.elapsed.as_secs_f64();
        outln!(w, "## Clone complete");
        outln!(w);
        outln!(w, "- **path**: `{}`", self.path);
        outln!(w, "- **branch**: `{}`", self.branch);
        outln!(w, "- **url**: `{}`", self.url);
        outln!(w, "- **objects**: {}", self.objects);
        if let Some(t) = self.object_types {
            outln!(
                w,
                "- **object types**: {} commits, {} trees, {} blobs, {} tags",
                t.commits,
                t.trees,
                t.blobs,
                t.tags
            );
        }
        outln!(w, "- **bytes**: {} ({size})", self.bytes);
        outln!(w, "- **branches**: {}", self.branches);
        outln!(w, "- **tags**: {}", self.tags);
        outln!(w, "- **files**: {}", self.files);
        outln!(
            w,
            "- **worktree bytes**: {} ({})",
            self.worktree_bytes,
            format_bytes(self.worktree_bytes)
        );
        outln!(w, "- **elapsed**: {secs:.2}s");
        if !self.phases.is_empty() {
            outln!(w);
            outln!(w, "| Phase | Count | Unit | Seconds |");
            outln!(w, "| --- | ---: | --- | ---: |");
            for p in &self.phases {
                outln!(
                    w,
                    "| {} | {} | {} | {:.2} |",
                    p.phase,
                    p.count,
                    p.unit,
                    p.seconds
                );
            }
        }
        Ok(())
    }
}

fn emit_no_checkout_note(mode: OutputMode, outcome: &LibCloneOutcome) {
    match outcome.checkout {
        CloneCheckout::CheckedOut(_) => {}
        CloneCheckout::EmptyRemote => {
            if mode == OutputMode::Human {
                eprintln!("note: the remote repository is empty.");
            } else if mode == OutputMode::Markdown {
                eprintln!("> The remote repository is empty.");
            }
        }
        CloneCheckout::UnbornDefaultBranch => {
            let branch = &outcome.branch;
            if mode == OutputMode::Human {
                eprintln!(
                    "warning: the remote default branch '{branch}' has no commits yet; no files were checked out."
                );
                if !outcome.branches_with_commits.is_empty() {
                    let names = outcome.branches_with_commits.join(", ");
                    eprintln!("note: remote branches with commits: {names}");
                    if let Some(first) = outcome.branches_with_commits.first() {
                        eprintln!("hint: run `grit switch {first}` to check one out");
                    }
                }
            } else if mode == OutputMode::Markdown {
                eprintln!(
                    "> **Warning:** the remote default branch `{branch}` has no commits yet; no files were checked out."
                );
                if !outcome.branches_with_commits.is_empty() {
                    let names = outcome
                        .branches_with_commits
                        .iter()
                        .map(|b| format!("`{b}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    eprintln!("> Remote branches with commits: {names}.");
                    if let Some(first) = outcome.branches_with_commits.first() {
                        eprintln!("> Try `grit switch {first}`.");
                    }
                }
            }
        }
    }
}

fn tag_fetch_mode_label(mode: CloneTags) -> &'static str {
    match mode {
        CloneTags::All => "all",
        CloneTags::Following => "following",
        CloneTags::None => "none",
    }
}

fn resolved_clone_tags(opts: &CloneRunOptions) -> Option<CloneTags> {
    if opts.no_tags {
        Some(CloneTags::None)
    } else {
        None
    }
}

fn effective_tag_fetch_mode(opts: &CloneRunOptions) -> CloneTags {
    resolved_clone_tags(opts).unwrap_or(if opts.shallow {
        CloneTags::Following
    } else {
        CloneTags::All
    })
}

pub fn run(opts: CloneRunOptions) -> Result<CloneOutcome> {
    let url = &opts.url;
    let mode = opts.mode;
    let dir = match &opts.dir {
        Some(d) => d.clone(),
        None => derive_clone_dir(url).map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let path = PathBuf::from(&dir);
    let dest_preexisted_empty = dest_preexisted_empty(&path);
    let started = Instant::now();

    // A clone always ends with a checkout, so the overall bar can size it.
    let mut term_progress = TerminalProgress::new(mode).with_overall(true);
    if term_progress.is_drawing() {
        let color = term_progress.uses_color();
        eprintln!(
            "{} {} {} {}",
            paint(color, BOLD, "Cloning"),
            paint(color, DIM, url),
            paint(color, DIM, "→"),
            paint(color, BOLD, &dir)
        );
    } else {
        progress(mode, &format!("Cloning into '{dir}' ..."));
    }
    match clone_into(&opts, &path, &dir, started, &mut term_progress) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let skip_cleanup = matches!(
                err.downcast_ref::<CloneError>(),
                Some(CloneError::DestNotEmpty(_))
            );
            if !skip_cleanup {
                cleanup_failed_clone(&path, dest_preexisted_empty);
            }
            Err(err)
        }
    }
}

fn clone_into(
    run: &CloneRunOptions,
    path: &Path,
    dir: &str,
    started: Instant,
    term_progress: &mut TerminalProgress,
) -> Result<CloneOutcome> {
    let factory = DefaultHttpClientFactory;
    let remote_name = run
        .origin
        .clone()
        .unwrap_or_else(|| grit_lib::remote::DEFAULT_REMOTE.to_owned());
    let tag_mode = effective_tag_fetch_mode(run);
    let clone_opts = CloneOptions {
        url: run.url.clone(),
        dest: path.to_path_buf(),
        environment: crate::context::environment(),
        remote_name: remote_name.clone(),
        initial_branch: "main".to_owned(),
        thread_policy: crate::context::thread_policy_for_repo(),
        depth: run.shallow.then_some(1),
        branch: None,
        single_branch: run.single_branch,
        tags: resolved_clone_tags(run),
    };
    let single_branch = clone_effective_single_branch(&clone_opts);
    let outcome = clone_repo(clone_opts, &mut *term_progress, Some(&factory))?;

    let (files, worktree_bytes, no_checkout_reason, branches_with_commits) =
        if let CloneCheckout::CheckedOut(oid) = outcome.checkout {
            let tree = context::commit_tree(&outcome.repo, &oid)?;
            checkout_between_trees_with_progress(&outcome.repo, None, &tree, &mut *term_progress)
                .context("could not check out files")?;
            let index = outcome
                .repo
                .index_handle()
                .load()
                .context("could not read index")?;
            let entries = index.entries();
            (
                u64::try_from(entries.len()).unwrap_or(u64::MAX),
                entries.iter().map(|e| u64::from(e.size)).sum(),
                None,
                None,
            )
        } else {
            emit_no_checkout_note(run.mode, &outcome);
            let reason = match outcome.checkout {
                CloneCheckout::EmptyRemote => Some("empty_remote".to_owned()),
                CloneCheckout::UnbornDefaultBranch => Some("unborn_default_branch".to_owned()),
                CloneCheckout::CheckedOut(_) => None,
            };
            let tips = (!outcome.branches_with_commits.is_empty())
                .then(|| outcome.branches_with_commits.clone());
            (0, 0, reason, tips)
        };
    let records = term_progress.phases().to_vec();
    let elapsed = started.elapsed();

    Ok(CloneOutcome {
        url: run.url.clone(),
        path: dir.to_owned(),
        remote: remote_name,
        single_branch,
        tag_fetch: tag_fetch_mode_label(tag_mode).to_owned(),
        branch: outcome.branch,
        objects: outcome.transfer.objects,
        bytes: outcome.transfer.bytes,
        branches: outcome.branches,
        tags: outcome.tags,
        files,
        object_types: outcome.transfer.kinds.map(|k| ObjectTypes {
            commits: k.commits,
            trees: k.trees,
            blobs: k.blobs,
            tags: k.tags,
        }),
        worktree_bytes,
        seconds: elapsed.as_secs_f64(),
        phases: records
            .iter()
            .map(|r| ClonePhase {
                phase: phase_json_name(r.phase).to_owned(),
                unit: unit_json_name(r.unit).to_owned(),
                count: r.count,
                seconds: r.elapsed.as_secs_f64(),
            })
            .collect(),
        elapsed,
        records,
        no_checkout_reason,
        branches_with_commits,
    })
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
            peak_rate: 4096.0,
        }
    }

    fn outcome(records: Vec<PhaseRecord>) -> CloneOutcome {
        CloneOutcome {
            url: "https://example.com/r.git".to_owned(),
            path: "r".to_owned(),
            remote: "origin".to_owned(),
            single_branch: false,
            tag_fetch: "all".to_owned(),
            branch: "main".to_owned(),
            objects: 1_234,
            bytes: 2 * 1024 * 1024,
            branches: 1,
            tags: 3,
            files: 42,
            object_types: Some(ObjectTypes {
                commits: 100,
                trees: 400,
                blobs: 734,
                tags: 0,
            }),
            worktree_bytes: 512 * 1024,
            seconds: 2.0,
            phases: Vec::new(),
            elapsed: Duration::from_secs(2),
            records,
            no_checkout_reason: None,
            branches_with_commits: None,
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
        let names: Vec<&str> = timeline(&records, Duration::from_secs(2))
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
        let (bar, legend) = time_bar(&records, Duration::from_secs(2), false);
        assert_eq!(bar.chars().count(), TIME_BAR_WIDTH);
        assert_eq!(bar.chars().filter(|&c| c == '█').count(), 30);
        assert!(legend.contains("receiving 1.5s"), "{legend}");
        assert!(legend.contains("checkout 500ms"), "{legend}");
    }

    #[test]
    fn plain_summary_reports_time_size_objects_refs_and_files() {
        let o = outcome(vec![rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            0,
            1000,
            2 * 1024 * 1024,
        )]);
        let mut out = Vec::new();
        render_summary(&o, &mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with("✓ Cloned into r on main in 2.0s\n"),
            "{text}"
        );
        assert!(
            text.contains("2.00 MiB received · 2.00 MiB/s avg · 4.00 KiB/s peak"),
            "{text}"
        );
        assert!(
            text.contains("1,234 · 100 commits · 400 trees · 734 blobs"),
            "{text}"
        );
        assert!(
            !text.contains("0 tags ·"),
            "zero type counts are omitted: {text}"
        );
        assert!(text.contains("1 branch · 3 tags"), "{text}");
        assert!(
            text.contains("42 files · 512 KiB in the work tree"),
            "{text}"
        );
        assert!(!text.contains('\x1b'), "no escapes without color: {text}");
    }

    #[test]
    fn summary_skips_empty_sections_for_empty_remote() {
        let mut o = outcome(Vec::new());
        o.objects = 0;
        o.bytes = 0;
        o.files = 0;
        o.object_types = None;
        let mut out = Vec::new();
        render_summary(&o, &mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("data"), "{text}");
        assert!(!text.contains("objects"), "{text}");
        assert!(!text.contains("files"), "{text}");
        assert!(text.contains("refs"), "{text}");
    }
}

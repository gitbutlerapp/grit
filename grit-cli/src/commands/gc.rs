//! `grit gc` — repository maintenance tasks (pack-refs, repack, prune, commit-graph, …).

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use grit_lib::config::git_date::approx::approxidate_careful_at;
use grit_lib::maintenance::{
    auto_gc_needed, gc, gc_auto, pack_refs, prune_loose_objects, repack_all, write_commit_graph,
    write_midx, AutoGcOutcome, AutoGcReason, CommitGraphOptions, GcLock, GcOptions, GcPhaseTimings,
    GcStats, MidxOptions, PackRefsOptions, PruneOptions, RepackOptions,
};
use grit_lib::objects::midx::MidxBitmapWriteOutcome;
use schemars::JsonSchema;
use serde::Serialize;
use time::OffsetDateTime;

use crate::context;
use crate::output::{outln, progress, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::ui::filesystem_path_for_user;

/// Result of `grit gc` and its task subcommands.
#[derive(Serialize, JsonSchema)]
pub struct GcOutcome {
    #[serde(skip)]
    pub quiet: bool,
    pub dry_run: bool,
    /// Decision of `grit gc --auto`; absent for a plain `grit gc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<GcAutoReport>,
    pub tasks: Vec<GcTaskReport>,
}

/// What `grit gc --auto` decided (JSON uses a `status` discriminator).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum GcAutoReport {
    /// Below `gc.auto` and `gc.autoPackLimit` (or `gc.auto=0`); nothing ran.
    NotNeeded,
    /// Another maintenance run holds `gc.pid`; nothing ran.
    AlreadyRunning {
        pid: Option<u64>,
        host: Option<String>,
    },
    /// The `pre-auto-gc` hook exited non-zero; nothing ran.
    DeclinedByHook { exit_code: i32 },
    /// Maintenance ran (or would run, with `--dry-run`).
    Ran {
        #[serde(flatten)]
        trigger: GcAutoTrigger,
    },
}

/// Threshold that triggered `grit gc --auto`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "trigger", rename_all = "kebab-case")]
pub enum GcAutoTrigger {
    /// Estimated loose objects above `gc.auto`.
    LooseObjects { estimate: usize, limit: usize },
    /// Packs without `.keep` above `gc.autoPackLimit`.
    Packs { count: usize, limit: usize },
}

impl From<AutoGcReason> for GcAutoTrigger {
    fn from(reason: AutoGcReason) -> Self {
        match reason {
            AutoGcReason::TooManyLoose { estimate, limit } => {
                Self::LooseObjects { estimate, limit }
            }
            AutoGcReason::TooManyPacks { count, limit } => Self::Packs { count, limit },
        }
    }
}

impl GcAutoReport {
    fn render_human_line(&self, w: &mut dyn Write, dry_run: bool) -> Result<()> {
        match self {
            Self::NotNeeded => outln!(
                w,
                "auto: nothing to do (below gc.auto and gc.autoPackLimit)."
            ),
            Self::AlreadyRunning { pid, host } => {
                let holder = match (pid, host) {
                    (Some(pid), Some(host)) => format!(" (process {pid} on {host})"),
                    (Some(pid), None) => format!(" (process {pid})"),
                    _ => String::new(),
                };
                outln!(w, "auto: skipped, maintenance already running{holder}.");
            }
            Self::DeclinedByHook { exit_code } => outln!(
                w,
                "auto: skipped, pre-auto-gc hook exited with status {exit_code}."
            ),
            Self::Ran { trigger } => {
                let verb = if dry_run { "would run" } else { "ran" };
                match trigger {
                    GcAutoTrigger::LooseObjects { estimate, limit } => outln!(
                        w,
                        "auto: {verb} for about {estimate} loose objects (gc.auto = {limit})."
                    ),
                    GcAutoTrigger::Packs { count, limit } => outln!(
                        w,
                        "auto: {verb} for {count} packs (gc.autoPackLimit = {limit})."
                    ),
                }
            }
        }
        Ok(())
    }

    fn render_markdown(&self, w: &mut dyn Write) -> Result<()> {
        let status = match self {
            Self::NotNeeded => "not-needed",
            Self::AlreadyRunning { .. } => "already-running",
            Self::DeclinedByHook { .. } => "declined-by-hook",
            Self::Ran { .. } => "ran",
        };
        outln!(w, "- **auto:** {status}");
        match self {
            Self::NotNeeded => {}
            Self::AlreadyRunning { pid, host } => {
                if let Some(pid) = pid {
                    outln!(w, "- **pid:** {pid}");
                }
                if let Some(host) = host {
                    outln!(w, "- **host:** {host}");
                }
            }
            Self::DeclinedByHook { exit_code } => outln!(w, "- **exit_code:** {exit_code}"),
            Self::Ran {
                trigger: GcAutoTrigger::LooseObjects { estimate, limit },
            } => {
                outln!(w, "- **trigger:** loose-objects");
                outln!(w, "- **estimate:** {estimate}");
                outln!(w, "- **limit:** {limit}");
            }
            Self::Ran {
                trigger: GcAutoTrigger::Packs { count, limit },
            } => {
                outln!(w, "- **trigger:** packs");
                outln!(w, "- **count:** {count}");
                outln!(w, "- **limit:** {limit}");
            }
        }
        Ok(())
    }
}

/// One maintenance task report (JSON uses a `task` discriminator).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "task", rename_all = "kebab-case")]
pub enum GcTaskReport {
    PackRefs {
        duration_ms: u64,
        refs_packed: usize,
        refs_pruned: usize,
        path: String,
    },
    ReflogExpire {
        duration_ms: u64,
        entries_pruned: usize,
    },
    Repack {
        duration_ms: u64,
        #[serde(flatten)]
        report: RepackSummary,
    },
    Prune {
        duration_ms: u64,
        pruned: usize,
        kept: usize,
    },
    CommitGraph {
        duration_ms: u64,
        commit_count: usize,
        path: String,
        removed_chain: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<String>,
    },
    Midx {
        duration_ms: u64,
        pack_count: usize,
        path: String,
        bitmap: String,
    },
}

#[derive(Serialize, JsonSchema)]
pub struct RepackSummary {
    pub loose_before: usize,
    pub packs_before: usize,
    pub objects_packed: Option<usize>,
    pub pack_bytes: Option<u64>,
    pub pack_path: Option<String>,
    pub removed_pack_files: usize,
    pub pruned_loose: usize,
    pub unreachable_loose_pruned: usize,
}

impl From<grit_lib::maintenance::RepackStats> for RepackSummary {
    fn from(stats: grit_lib::maintenance::RepackStats) -> Self {
        Self {
            loose_before: stats.loose_before,
            packs_before: stats.packs_before,
            objects_packed: stats.objects_packed,
            pack_bytes: stats.pack_bytes,
            pack_path: stats.pack_path.as_deref().map(filesystem_path_for_user),
            removed_pack_files: stats.removed_pack_files,
            pruned_loose: stats.pruned_loose,
            unreachable_loose_pruned: stats.unreachable_loose_pruned,
        }
    }
}

impl RepackSummary {
    fn render_markdown_fields(&self, w: &mut dyn Write) -> Result<()> {
        outln!(w, "- **loose_before:** {}", self.loose_before);
        outln!(w, "- **packs_before:** {}", self.packs_before);
        if let Some(n) = self.objects_packed {
            outln!(w, "- **objects_packed:** {n}");
        }
        if let Some(bytes) = self.pack_bytes {
            outln!(w, "- **pack_bytes:** {bytes}");
        }
        if let Some(path) = &self.pack_path {
            outln!(w, "- **pack_path:** `{path}`");
        }
        outln!(w, "- **removed_pack_files:** {}", self.removed_pack_files);
        outln!(w, "- **pruned_loose:** {}", self.pruned_loose);
        outln!(
            w,
            "- **unreachable_loose_pruned:** {}",
            self.unreachable_loose_pruned
        );
        Ok(())
    }
}

impl HumanRender for GcOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.quiet {
            return Ok(());
        }
        if self.dry_run {
            outln!(w, "Dry run: no changes written.");
        }
        if let Some(auto) = &self.auto {
            auto.render_human_line(w, self.dry_run)?;
        }
        for task in &self.tasks {
            task.render_human_line(w, self.dry_run)?;
        }
        Ok(())
    }
}

impl MarkdownRender for GcOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Garbage collection\n");
        outln!(w, "- **dry_run:** {}", self.dry_run);
        if let Some(auto) = &self.auto {
            auto.render_markdown(w)?;
        }
        outln!(w, "\n### Tasks\n");
        for task in &self.tasks {
            task.render_markdown(w)?;
        }
        Ok(())
    }
}

impl GcTaskReport {
    fn render_human_line(&self, w: &mut dyn Write, dry_run: bool) -> Result<()> {
        match self {
            Self::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "pack-refs: would pack {refs_packed} refs ({refs_pruned} loose removed) → {path} ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "pack-refs: packed {refs_packed} refs ({refs_pruned} loose removed) → {path} ({duration_ms} ms)."
                    );
                }
            }
            Self::ReflogExpire {
                duration_ms,
                entries_pruned,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "reflog-expire: would remove {entries_pruned} entries ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "reflog-expire: removed {entries_pruned} entries ({duration_ms} ms)."
                    );
                }
            }
            Self::Repack {
                duration_ms,
                report,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "repack: would pack {} objects ({} loose, {} pack files now) ({duration_ms} ms).",
                        report.objects_packed.unwrap_or(0),
                        report.loose_before,
                        report.packs_before
                    );
                } else if let Some(path) = &report.pack_path {
                    outln!(
                        w,
                        "repack: packed {} objects into {path} ({} ms).",
                        report.objects_packed.unwrap_or(0),
                        duration_ms
                    );
                } else {
                    outln!(w, "repack: nothing to pack ({duration_ms} ms).");
                }
            }
            Self::Prune {
                duration_ms,
                pruned,
                kept,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "prune: would remove {pruned} unreachable loose objects ({kept} kept, {duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "prune: removed {pruned} unreachable loose objects ({kept} kept, {duration_ms} ms)."
                    );
                }
            }
            Self::CommitGraph {
                duration_ms,
                skipped: Some(reason),
                ..
            } => {
                if dry_run {
                    outln!(
                        w,
                        "commit-graph: would skip, parents are rewritten ({reason}); any existing graph would be removed ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "commit-graph: skipped, parents are rewritten ({reason}); any existing graph removed ({duration_ms} ms)."
                    );
                }
            }
            Self::CommitGraph {
                duration_ms,
                commit_count,
                path,
                ..
            } => {
                if dry_run {
                    outln!(
                        w,
                        "commit-graph: would write {commit_count} commits to {path} ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "commit-graph: wrote {commit_count} commits to {path} ({duration_ms} ms)."
                    );
                }
            }
            Self::Midx {
                duration_ms,
                pack_count,
                path,
                bitmap,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "midx: would index {pack_count} packs at {path} (bitmap: {bitmap}, {duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "midx: indexed {pack_count} packs at {path} (bitmap: {bitmap}, {duration_ms} ms)."
                    );
                }
            }
        }
        Ok(())
    }

    fn render_markdown(&self, w: &mut dyn Write) -> Result<()> {
        match self {
            Self::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path,
            } => {
                outln!(w, "#### pack-refs\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **refs_packed:** {refs_packed}");
                outln!(w, "- **refs_pruned:** {refs_pruned}");
                outln!(w, "- **path:** `{path}`");
                outln!(w);
            }
            Self::ReflogExpire {
                duration_ms,
                entries_pruned,
            } => {
                outln!(w, "#### reflog-expire\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **entries_pruned:** {entries_pruned}");
                outln!(w);
            }
            Self::Repack {
                duration_ms,
                report,
            } => {
                outln!(w, "#### repack\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                report.render_markdown_fields(w)?;
                outln!(w);
            }
            Self::Prune {
                duration_ms,
                pruned,
                kept,
            } => {
                outln!(w, "#### prune\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **pruned:** {pruned}");
                outln!(w, "- **kept:** {kept}");
                outln!(w);
            }
            Self::CommitGraph {
                duration_ms,
                commit_count,
                path,
                removed_chain,
                skipped,
            } => {
                outln!(w, "#### commit-graph\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **commit_count:** {commit_count}");
                outln!(w, "- **path:** `{path}`");
                outln!(w, "- **removed_chain:** {removed_chain}");
                if let Some(reason) = skipped {
                    outln!(w, "- **skipped:** {reason}");
                }
                outln!(w);
            }
            Self::Midx {
                duration_ms,
                pack_count,
                path,
                bitmap,
            } => {
                outln!(w, "#### midx\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **pack_count:** {pack_count}");
                outln!(w, "- **path:** `{path}`");
                outln!(w, "- **bitmap:** {bitmap}");
                outln!(w);
            }
        }
        Ok(())
    }
}

fn timed<F, T>(mode: OutputMode, label: &str, f: F) -> Result<(T, Duration)>
where
    F: FnOnce() -> Result<T>,
{
    progress(mode, label);
    let start = Instant::now();
    let out = f()?;
    Ok((out, start.elapsed()))
}

fn midx_bitmap_label(outcome: MidxBitmapWriteOutcome) -> &'static str {
    match outcome {
        MidxBitmapWriteOutcome::Written => "written",
        MidxBitmapWriteOutcome::NotRequested => "not-requested",
        MidxBitmapWriteOutcome::SkippedIncrementalLayer => "skipped-incremental",
        _ => "other",
    }
}

fn tasks_from_gc_stats(stats: GcStats, timings: GcPhaseTimings) -> Vec<GcTaskReport> {
    let mut tasks = Vec::new();
    if let Some(pack) = stats.pack_refs {
        tasks.push(GcTaskReport::PackRefs {
            duration_ms: timings.pack_refs_ms,
            refs_packed: pack.refs_packed,
            refs_pruned: pack.refs_pruned,
            path: filesystem_path_for_user(&pack.path),
        });
    }
    tasks.push(GcTaskReport::ReflogExpire {
        duration_ms: timings.reflog_ms,
        entries_pruned: stats.reflog_entries_pruned,
    });
    tasks.push(GcTaskReport::Repack {
        duration_ms: timings.repack_ms,
        report: stats.repack.into(),
    });
    if stats.prune_ran {
        tasks.push(GcTaskReport::Prune {
            duration_ms: timings.prune_ms,
            pruned: stats.prune.pruned,
            kept: stats.prune.kept,
        });
    }
    if let Some(graph) = stats.commit_graph {
        tasks.push(GcTaskReport::CommitGraph {
            duration_ms: timings.commit_graph_ms,
            commit_count: graph.commit_count,
            path: filesystem_path_for_user(&graph.path),
            removed_chain: graph.removed_chain,
            skipped: graph.skipped.map(|reason| reason.as_str().to_owned()),
        });
    }
    tasks
}

pub fn run_full(dry_run: bool, quiet: bool, mode: OutputMode) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if !quiet {
        progress(mode, "Running garbage collection…");
    }
    let opts = GcOptions::full_run(dry_run, now);
    let stats = gc(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    let timings = stats.phase_timings;
    let tasks = tasks_from_gc_stats(stats, timings);
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks,
    })
}

/// `grit gc --auto`: run maintenance only when `gc.auto` / `gc.autoPackLimit` say it is due.
pub fn run_auto(dry_run: bool, quiet: bool, now: OffsetDateTime) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let opts = GcOptions::full_run(dry_run, now.unix_timestamp());
    let outcome = gc_auto(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (auto, tasks) = match outcome {
        AutoGcOutcome::NotNeeded => (GcAutoReport::NotNeeded, Vec::new()),
        AutoGcOutcome::AlreadyRunning(holder) => (
            GcAutoReport::AlreadyRunning {
                pid: holder.pid,
                host: holder.host,
            },
            Vec::new(),
        ),
        AutoGcOutcome::DeclinedByHook { status, .. } => (
            GcAutoReport::DeclinedByHook { exit_code: status },
            Vec::new(),
        ),
        AutoGcOutcome::Ran { reason, stats } => {
            let timings = stats.phase_timings;
            (
                GcAutoReport::Ran {
                    trigger: reason.into(),
                },
                tasks_from_gc_stats(*stats, timings),
            )
        }
        _ => (GcAutoReport::NotNeeded, Vec::new()),
    };
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: Some(auto),
        tasks,
    })
}

/// Start automatic maintenance after a command that wrote objects, when it is due.
///
/// Honors `maintenance.auto`, `gc.auto`, `gc.autoPackLimit` and `gc.autoDetach`. Detached runs
/// re-execute this binary as `grit gc --auto --quiet` in the background; otherwise maintenance
/// runs here before the command exits. Never fails the command that triggered it: problems are
/// reported on stderr in human mode only.
pub fn run_auto_after_command(mode: OutputMode, now: OffsetDateTime) {
    let Ok(repo) = context::discover() else {
        return;
    };
    let Ok(config) = repo.config() else {
        return;
    };
    if !config.maintenance_auto() {
        return;
    }
    let Ok(Some(_)) = auto_gc_needed(&repo) else {
        return;
    };
    if GcLock::current_holder(&repo, now.unix_timestamp()).is_some() {
        return;
    }
    let human = mode == OutputMode::Human;
    if config.gc_auto_detach() {
        match spawn_detached_auto_gc() {
            Ok(()) => {
                if human {
                    eprintln!("Packing the repository in the background (gc.auto); see `grit gc`.");
                }
                return;
            }
            Err(first) => {
                #[cfg(windows)]
                {
                    if spawn_detached_auto_gc_impl(detach_creation_flags(false)).is_ok() {
                        if human {
                            eprintln!(
                                "Packing the repository in the background (gc.auto); see `grit gc`."
                            );
                        }
                        return;
                    }
                }
                if human {
                    eprintln!("warning: could not start background maintenance: {first}");
                }
            }
        }
    }
    let opts = GcOptions::full_run(false, now.unix_timestamp());
    match gc_auto(&repo, &opts) {
        Ok(AutoGcOutcome::Ran { .. }) if human => {
            eprintln!("Packed the repository (gc.auto); see `grit gc`.");
        }
        Err(err) if human => eprintln!("warning: automatic maintenance failed: {err}"),
        _ => {}
    }
}

/// Windows: allow the child to leave the caller's job object (PowerShell, CI, Defender
/// harnesses). Without this, the parent can remain blocked until background gc finishes even
/// after `spawn` returns.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

fn spawn_detached_auto_gc() -> std::io::Result<()> {
    spawn_detached_auto_gc_impl(detach_creation_flags(true)).map(|_| ())
}

#[cfg(windows)]
fn detach_creation_flags(breakaway_from_job: bool) -> u32 {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if breakaway_from_job {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    flags
}

#[cfg(unix)]
fn detach_creation_flags(_breakaway_from_job: bool) -> u32 {
    0
}

fn spawn_detached_auto_gc_impl(
    #[cfg_attr(not(windows), allow(unused_variables))] creation_flags: u32,
) -> std::io::Result<u32> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["gc", "--auto", "--quiet"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Own process group, so a Ctrl-C aimed at the user's shell does not stop it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(creation_flags);
    }
    let child = cmd.spawn()?;
    let pid = child.id();
    if pid == 0 {
        return Err(std::io::Error::other(
            "background maintenance spawn returned pid 0",
        ));
    }
    Ok(pid)
}

pub fn run_repack(
    bitmaps: bool,
    force: bool,
    dry_run: bool,
    quiet: bool,
    mode: OutputMode,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = RepackOptions::with_dry_run(dry_run);
    opts.write_bitmap = bitmaps;
    opts.force_recompress = force;
    let (stats, elapsed) = timed(mode, "Repacking objects…", || {
        repack_all(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::Repack {
            duration_ms: elapsed.as_millis() as u64,
            report: stats.into(),
        }],
    })
}

pub fn run_commit_graph(dry_run: bool, quiet: bool, mode: OutputMode) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = CommitGraphOptions::with_changed_paths(true);
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(mode, "Writing commit-graph…", || {
        write_commit_graph(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::CommitGraph {
            duration_ms: elapsed.as_millis() as u64,
            commit_count: report.commit_count,
            path: filesystem_path_for_user(&report.path),
            removed_chain: report.removed_chain,
            skipped: report.skipped.map(|reason| reason.as_str().to_owned()),
        }],
    })
}

pub fn run_midx(dry_run: bool, quiet: bool, mode: OutputMode) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = MidxOptions::default();
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(mode, "Writing multi-pack-index…", || {
        write_midx(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::Midx {
            duration_ms: elapsed.as_millis() as u64,
            pack_count: report.pack_count,
            path: filesystem_path_for_user(&report.path),
            bitmap: midx_bitmap_label(report.bitmap).to_owned(),
        }],
    })
}

pub fn run_pack_refs(dry_run: bool, quiet: bool, mode: OutputMode) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = PackRefsOptions::default();
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(mode, "Packing refs…", || {
        pack_refs(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::PackRefs {
            duration_ms: elapsed.as_millis() as u64,
            refs_packed: report.refs_packed,
            refs_pruned: report.refs_pruned,
            path: filesystem_path_for_user(&report.path),
        }],
    })
}

pub fn run_prune(
    older_than: Option<&str>,
    dry_run: bool,
    quiet: bool,
    mode: OutputMode,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let cutoff = older_than.map(parse_older_than).transpose()?.flatten();
    let (stats, elapsed) = timed(mode, "Pruning unreachable loose objects…", || {
        let mut prune_opts = PruneOptions::with_older_than(cutoff);
        prune_opts.dry_run = dry_run;
        prune_loose_objects(&repo, &prune_opts).map_err(|e| anyhow::anyhow!("{e}"))
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::Prune {
            duration_ms: elapsed.as_millis() as u64,
            pruned: stats.pruned,
            kept: stats.kept,
        }],
    })
}

fn parse_older_than(spec: &str) -> Result<Option<SystemTime>> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut err = 0;
    let sec = approxidate_careful_at(spec.trim(), now, Some(&mut err));
    if err != 0 {
        anyhow::bail!("invalid date for --older-than: '{spec}'");
    }
    Ok(SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(sec as u64)))
}

/// Legacy entry: full gc with dry-run / quiet flags.
pub fn run(dry_run: bool, quiet: bool, mode: OutputMode) -> Result<GcOutcome> {
    run_full(dry_run, quiet, mode)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn gc_paths_strip_verbatim_prefix_for_user_output() {
        let mut stats = grit_lib::maintenance::RepackStats::default();
        stats.pack_path = Some(PathBuf::from(
            r"\\?\C:\repo\.git\objects\pack\pack-abc.pack",
        ));
        let summary = RepackSummary::from(stats);
        let pack_path = summary.pack_path.expect("pack_path");
        assert!(!pack_path.contains(r"\\?\"));
        assert_eq!(pack_path, "C:/repo/.git/objects/pack/pack-abc.pack");

        let packed_refs =
            filesystem_path_for_user(std::path::Path::new(r"\\?\C:\repo\.git\packed-refs"));
        assert_eq!(packed_refs, "C:/repo/.git/packed-refs");

        let task = GcTaskReport::PackRefs {
            duration_ms: 4,
            refs_packed: 2,
            refs_pruned: 2,
            path: packed_refs,
        };
        let mut buf = Vec::new();
        task.render_human_line(&mut buf, false).expect("render");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(!line.contains(r"\\?\"));
        assert!(line.contains("C:/repo/.git/packed-refs"));
    }

    #[test]
    fn gc_repack_dry_run_human_line_matches_repack_command() {
        let report = RepackSummary {
            loose_before: 307,
            packs_before: 0,
            objects_packed: Some(307),
            pack_bytes: None,
            pack_path: None,
            removed_pack_files: 0,
            pruned_loose: 0,
            unreachable_loose_pruned: 0,
        };
        let task = GcTaskReport::Repack {
            duration_ms: 268,
            report,
        };
        let mut buf = Vec::new();
        task.render_human_line(&mut buf, true).expect("render");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(line.contains("repack: would pack 307 objects (307 loose, 0 pack files now)"));
        assert!(!line.contains("nothing to pack"));
    }
}

//! `grit gc` — repository maintenance tasks (pack-refs, repack, prune, commit-graph, …).

use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use grit_lib::config::git_date::approx::approxidate_careful_at;
use grit_lib::config::git_path::user_display_path;
use grit_lib::maintenance::{
    AutoGcOutcome, CommitGraphOptions, DropExpiry, DropPromisedBlobsOptions, GcOptions,
    MidxOptions, OffloadOptions, PackRefsOptions, PruneOptions, RepackOptions,
};
use grit_lib::objects::midx::MidxBitmapWriteOutcome;
use grit_lib::progress::ProgressSink;
use time::OffsetDateTime;

use crate::context;

pub use crate::render::gc::*;

fn timed<F, T>(show_progress: bool, label: &str, f: F) -> Result<(T, Duration)>
where
    F: FnOnce() -> Result<T>,
{
    if show_progress {
        eprintln!("{label}");
    }
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

pub fn run_full(dry_run: bool, quiet: bool, show_progress: bool) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if !quiet && show_progress {
        eprintln!("Running garbage collection…");
    }
    let opts = GcOptions::full_run(dry_run, now);
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let stats = repo
        .maintenance()
        .gc(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(crate::json_error::map_lib_error)?;
    let tasks = crate::render::gc::task_reports_from_gc_stats(stats);
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks,
    })
}

pub fn run_auto(dry_run: bool, quiet: bool, now: OffsetDateTime) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let opts = GcOptions::full_run(dry_run, now.unix_timestamp());
    let outcome = repo
        .maintenance()
        .gc_auto(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .map_err(crate::json_error::map_lib_error)?;
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
        AutoGcOutcome::Ran { reason, stats } => (
            GcAutoReport::Ran {
                trigger: reason.into(),
            },
            crate::render::gc::task_reports_from_gc_stats(*stats),
        ),
        _ => (GcAutoReport::NotNeeded, Vec::new()),
    };
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: Some(auto),
        tasks,
    })
}

/// Auto maintenance after object-writing commands (`maintenance.auto`, `gc.auto`, …).
pub fn run_auto_after_command(
    progress: &mut dyn ProgressSink,
    show_stderr: bool,
    now: OffsetDateTime,
) {
    let Ok(repo) = context::discover() else {
        return;
    };
    let Ok(config) = repo.config().snapshot() else {
        return;
    };
    if !config.maintenance_auto() {
        return;
    }
    let Ok(Some(_)) = repo.maintenance().auto_gc_needed() else {
        return;
    };
    if repo
        .maintenance()
        .lock_holder(now.unix_timestamp())
        .is_some()
    {
        return;
    }
    if config.gc_auto_detach() {
        match spawn_detached_auto_gc() {
            Ok(()) => {
                if show_stderr {
                    eprintln!("Packing the repository in the background (gc.auto); see `grit gc`.");
                }
                return;
            }
            Err(err) if show_stderr => {
                eprintln!("warning: could not start background maintenance: {err}");
            }
            Err(_) => {}
        }
    }
    let opts = GcOptions::full_run(false, now.unix_timestamp());
    match repo
        .maintenance()
        .gc_auto(&opts, progress, &grit_lib::progress::CancelToken::never())
    {
        Ok(AutoGcOutcome::Ran { .. }) if show_stderr => {
            eprintln!("Packed the repository (gc.auto); see `grit gc`.");
        }
        Err(err) if show_stderr => eprintln!("warning: automatic maintenance failed: {err}"),
        _ => {}
    }
}

fn spawn_detached_auto_gc() -> std::io::Result<()> {
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
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        };
        // Job-aware callers may wait for every process in their job after commit exits.
        // Leave that job so their wait does not include background maintenance.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        stop_std_handle_inheritance();
    }
    cmd.spawn().map(drop)
}

#[cfg(windows)]
fn stop_std_handle_inheritance() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: std handles belong to this process.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

pub fn run_repack(
    bitmaps: bool,
    force: bool,
    dry_run: bool,
    quiet: bool,
    show_progress: bool,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = RepackOptions::with_dry_run(dry_run);
    opts.write_bitmap = bitmaps;
    opts.force_recompress = force;
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let (stats, elapsed) = timed(show_progress, "Repacking objects…", || {
        repo.maintenance()
            .repack_all(
                &opts,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(crate::json_error::map_lib_error)
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

pub fn run_commit_graph(dry_run: bool, quiet: bool, show_progress: bool) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = CommitGraphOptions::with_changed_paths(true);
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(show_progress, "Writing commit-graph…", || {
        repo.maintenance()
            .write_commit_graph(&opts)
            .map_err(crate::json_error::map_lib_error)
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::CommitGraph {
            duration_ms: elapsed.as_millis() as u64,
            commit_count: report.commit_count,
            path: user_display_path(&report.path),
            removed_chain: report.removed_chain,
            skipped: report.skipped.map(|reason| reason.as_str().to_owned()),
        }],
    })
}

pub fn run_midx(dry_run: bool, quiet: bool, show_progress: bool) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = MidxOptions::default();
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(show_progress, "Writing multi-pack-index…", || {
        repo.maintenance()
            .write_midx(&opts)
            .map_err(crate::json_error::map_lib_error)
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::Midx {
            duration_ms: elapsed.as_millis() as u64,
            pack_count: report.pack_count,
            path: user_display_path(&report.path),
            bitmap: midx_bitmap_label(report.bitmap).to_owned(),
        }],
    })
}

pub fn run_pack_refs(dry_run: bool, quiet: bool, show_progress: bool) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let mut opts = PackRefsOptions::default();
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(show_progress, "Packing refs…", || {
        repo.maintenance()
            .pack_refs(&opts)
            .map_err(crate::json_error::map_lib_error)
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::PackRefs {
            duration_ms: elapsed.as_millis() as u64,
            refs_packed: report.refs_packed,
            refs_pruned: report.refs_pruned,
            path: user_display_path(&report.path),
        }],
    })
}

pub fn run_prune(
    older_than: Option<&str>,
    dry_run: bool,
    quiet: bool,
    show_progress: bool,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let cutoff = older_than.map(parse_older_than).transpose()?.flatten();
    let (stats, elapsed) = timed(
        show_progress,
        "Pruning unreachable loose objects…",
        || {
            let mut prune_opts = PruneOptions::with_older_than(cutoff);
            prune_opts.dry_run = dry_run;
            repo.maintenance()
                .prune_loose_objects(&prune_opts)
                .map_err(crate::json_error::map_lib_error)
        },
    )?;
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

/// Arguments of `grit gc offload`.
pub struct OffloadArgs {
    /// `--to`: the promisor remote.
    pub remote: String,
    /// `--blobs`: `none` or a size; `None` uses the remote's partialCloneFilter.
    pub blobs: Option<String>,
    /// `--lop-dir`: the promisor's git directory.
    pub lop_dir: Option<std::path::PathBuf>,
    /// `--configure-lop`.
    pub configure_lop: bool,
    /// `--dry-run` (on `gc` or `gc offload`).
    pub dry_run: bool,
}

pub fn run_offload(
    args: OffloadArgs,
    quiet: bool,
    show_progress: bool,
    now: OffsetDateTime,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let filter = args
        .blobs
        .as_deref()
        .map(super::clone::blobs_filter_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid --blobs value: {e}"))?;
    let mut opts = OffloadOptions::new(args.remote, now.unix_timestamp());
    opts.filter = filter;
    opts.lop_git_dir = args.lop_dir;
    opts.configure_lop = args.configure_lop;
    opts.dry_run = args.dry_run;
    let (report, elapsed) = timed(show_progress, "Offloading large blobs…", || {
        repo.maintenance()
            .offload_blobs(&opts)
            .map_err(crate::json_error::map_lib_error)
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run: args.dry_run,
        auto: None,
        tasks: vec![GcTaskReport::Offload {
            duration_ms: elapsed.as_millis() as u64,
            report: report.into(),
        }],
    })
}

pub fn run_drop_large_blobs(
    min_size: Option<&str>,
    older_than: Option<&str>,
    dry_run: bool,
    quiet: bool,
    show_progress: bool,
    now: OffsetDateTime,
) -> Result<GcOutcome> {
    let repo = context::discover()?;
    let config = repo
        .config()
        .snapshot()
        .map_err(crate::json_error::map_lib_error)?;
    let min_size = min_size
        .map(|size| {
            super::clone::blobs_filter_spec(size)
                .ok()
                .and_then(|f| f.blob_limit_bytes())
                .ok_or_else(|| anyhow::anyhow!("invalid --min-size value: '{size}'"))
        })
        .transpose()?;
    let expiry = older_than
        .map(|spec| {
            DropExpiry::parse(spec, now)
                .ok_or_else(|| anyhow::anyhow!("invalid date for --older-than: '{spec}'"))
        })
        .transpose()?;
    let Some(mut opts) = DropPromisedBlobsOptions::from_config(&config, now, min_size, expiry)
        .map_err(crate::json_error::map_lib_error)?
    else {
        if older_than.is_some() {
            anyhow::bail!("--older-than never keeps every blob; nothing to drop");
        }
        anyhow::bail!(
            "maintenance.drop-large-blobs.expire is 'never'; give --older-than to run anyway"
        );
    };
    opts.dry_run = dry_run;
    let (report, elapsed) = timed(show_progress, "Dropping large promised blobs…", || {
        repo.maintenance()
            .drop_promised_blobs(&opts)
            .map_err(crate::json_error::map_lib_error)
    })?;
    Ok(GcOutcome {
        quiet,
        dry_run,
        auto: None,
        tasks: vec![GcTaskReport::DropLargeBlobs {
            duration_ms: elapsed.as_millis() as u64,
            report: report.into(),
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

pub fn run(dry_run: bool, quiet: bool, show_progress: bool) -> Result<GcOutcome> {
    run_full(dry_run, quiet, show_progress)
}

/// Dispatch parsed CLI args for `Gc`.
pub fn dispatch(
    args: crate::args::GcArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;
    use crate::dispatch::{emit_success, CommandEnv};
    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let GcArgs {
        dry_run,
        auto,
        quiet,
        action,
    } = args;
    let show_progress = opts.show_progress();
    if auto && action.is_some() {
        bail!("`--auto` cannot be combined with a gc subcommand");
    }
    let outcome = match action {
        None if auto => commands::gc::run_auto(dry_run, quiet, ctx.now)?,
        None => commands::gc::run(dry_run, quiet, show_progress)?,
        Some(GcAction::Repack {
            bitmaps,
            force,
            dry_run: repack_dry,
        }) => {
            commands::gc::run_repack(bitmaps, force, dry_run || repack_dry, quiet, show_progress)?
        }
        Some(GcAction::CommitGraph { dry_run: sub_dry }) => {
            commands::gc::run_commit_graph(dry_run || sub_dry, quiet, show_progress)?
        }
        Some(GcAction::Midx { dry_run: sub_dry }) => {
            commands::gc::run_midx(dry_run || sub_dry, quiet, show_progress)?
        }
        Some(GcAction::PackRefs { dry_run: sub_dry }) => {
            commands::gc::run_pack_refs(dry_run || sub_dry, quiet, show_progress)?
        }
        Some(GcAction::Prune {
            older_than,
            dry_run: sub_dry,
        }) => commands::gc::run_prune(
            older_than.as_deref(),
            dry_run || sub_dry,
            quiet,
            show_progress,
        )?,
        Some(GcAction::Offload {
            to,
            blobs,
            lop_dir,
            configure_lop,
            dry_run: sub_dry,
        }) => commands::gc::run_offload(
            commands::gc::OffloadArgs {
                remote: to,
                blobs,
                lop_dir,
                configure_lop,
                dry_run: dry_run || sub_dry,
            },
            quiet,
            show_progress,
            ctx.now,
        )?,
        Some(GcAction::DropLargeBlobs {
            min_size,
            older_than,
            dry_run: sub_dry,
        }) => commands::gc::run_drop_large_blobs(
            min_size.as_deref(),
            older_than.as_deref(),
            dry_run || sub_dry,
            quiet,
            show_progress,
            ctx.now,
        )?,
    };
    emit_success(outcome, opts, ctx)
}

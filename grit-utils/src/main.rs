use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grit_utils::binary::{require_hyperfine, resolve_binary};
use grit_utils::compare::compare_files;
use grit_utils::fixture::{remove_dir_robust, scratch_dir};
use grit_utils::render::{render_markdown, render_text};
use grit_utils::scenarios::{
    run_add_suite, run_hot_path_suite, run_prepare_add, run_prepare_merge, run_prepare_pick,
    run_prepare_pick_series, run_prepare_switch, run_status_suite, RunConfig,
};
use grit_utils::schema::BenchReport;
use time::OffsetDateTime;

#[derive(Parser)]
#[command(name = "grit-bench", about = "Benchmark grit vs git via hyperfine")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,

    #[arg(long, global = true)]
    grit: Option<PathBuf>,

    #[arg(long, global = true)]
    git: Option<PathBuf>,

    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,

    #[arg(short, long, global = true)]
    output: Option<PathBuf>,

    #[arg(long, global = true, default_value = "3")]
    warmup: u32,

    #[arg(long, global = true, default_value = "5")]
    min_runs: u32,

    /// RFC3339 timestamp for the report (default: now UTC).
    #[arg(long, global = true)]
    timestamp: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Benchmark `status` at various repo sizes
    Status {
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
    /// Benchmark `add` at various repo sizes
    Add {
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
    /// Switch / pick / merge hot-path scenarios (L/H sizes by default)
    HotPaths {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000, 100_000])]
        sizes: Vec<usize>,
        /// Enable `core.fsmonitor` with a trivial hook v2 script in the fixture.
        #[arg(long)]
        fsmonitor_fixture: bool,
    },
    /// Run status (dirty + clean) and add benchmarks
    All {
        #[arg(long, value_delimiter = ',', default_values_t = vec![100, 1_000, 10_000, 50_000])]
        sizes: Vec<usize>,
    },
    /// Compare two JSON reports; exit non-zero when ratios differ beyond tolerance
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
        #[arg(long, default_value = "0.10")]
        tolerance: f64,
    },
    /// Internal: hyperfine `--prepare` hook for add benchmarks
    #[command(hide = true)]
    PrepareAdd {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareSwitch {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PreparePick {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareMerge {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PreparePickSeries {
        #[arg(long)]
        git: PathBuf,
    },
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Markdown,
    Json,
}

fn parse_timestamp(cli: &Cli) -> Result<OffsetDateTime> {
    if let Some(ts) = &cli.timestamp {
        if let Ok(parsed) =
            OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
        {
            return Ok(parsed);
        }
        let fmt = time::format_description::parse_borrowed::<1>(
            "[year]-[month]-[day] [hour]:[minute]:[second]",
        )
        .context("build timestamp format")?;
        return OffsetDateTime::parse(ts, &fmt).context("parse --timestamp");
    }
    Ok(OffsetDateTime::now_utc())
}

fn run_config(cli: &Cli, isolated_config: bool) -> RunConfig {
    RunConfig {
        warmup: cli.warmup,
        min_runs: cli.min_runs,
        prepare_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
        isolated_config,
    }
}

fn write_output(cli: &Cli, body: &str) -> Result<()> {
    if let Some(path) = &cli.output {
        std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
        eprintln!("Results written to {}", path.display());
    } else {
        print!("{body}");
    }
    Ok(())
}

fn render_report(format: &OutputFormat, report: &BenchReport) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_text(report)),
        OutputFormat::Markdown => Ok(render_markdown(report)),
        OutputFormat::Json => Ok(serde_json::to_string_pretty(report)?),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Cmd::PrepareAdd { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_add(&git);
        }
        Cmd::PrepareSwitch { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_switch(&git);
        }
        Cmd::PreparePick { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_pick(&git);
        }
        Cmd::PrepareMerge { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_merge(&git);
        }
        Cmd::PreparePickSeries { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_pick_series(&git);
        }
        _ => {}
    }

    if let Cmd::Compare {
        baseline,
        candidate,
        tolerance,
    } = &cli.command
    {
        let mismatches = compare_files(baseline, candidate, *tolerance)?;
        if mismatches.is_empty() {
            eprintln!("All scenario ratios within tolerance {tolerance}");
            return Ok(());
        }
        eprintln!("Ratio drift exceeds tolerance {tolerance}:");
        for m in &mismatches {
            eprintln!(
                "  {}: {:.4} vs {:.4} (delta {:.4})",
                m.scenario_id, m.ratio_a, m.ratio_b, m.delta
            );
        }
        std::process::exit(1);
    }

    let git = resolve_binary("git", cli.git.as_deref())?;
    let grit = resolve_binary("grit", cli.grit.as_deref())?;
    let hyperfine = require_hyperfine()?;
    let timestamp = parse_timestamp(&cli)?;

    eprintln!(
        "git:  {} ({})",
        git.display(),
        grit_utils::binary::tool_version(&git)
    );
    eprintln!(
        "grit: {} ({})",
        grit.display(),
        grit_utils::binary::tool_version(&grit)
    );
    eprintln!("hyperfine: {}", hyperfine.display());
    eprintln!();

    let report = match &cli.command {
        Cmd::Status { sizes } => {
            eprintln!("Running status benchmarks...");
            let cfg = run_config(&cli, false);
            run_status_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::Add { sizes } => {
            eprintln!("Running add benchmarks...");
            let cfg = run_config(&cli, false);
            run_add_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::HotPaths {
            sizes,
            fsmonitor_fixture,
        } => {
            eprintln!("Running hot-path benchmarks...");
            let cfg = run_config(&cli, true);
            run_hot_path_suite(
                &hyperfine,
                &git,
                &grit,
                &cfg,
                sizes,
                *fsmonitor_fixture,
                timestamp,
            )?
        }
        Cmd::All { sizes } => {
            eprintln!("Running all benchmarks...");
            let cfg = run_config(&cli, false);
            let mut status = run_status_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?;
            let add = run_add_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?;
            status.scenarios.extend(add.scenarios);
            status
        }
        Cmd::Compare { .. }
        | Cmd::PrepareAdd { .. }
        | Cmd::PrepareSwitch { .. }
        | Cmd::PreparePick { .. }
        | Cmd::PrepareMerge { .. }
        | Cmd::PreparePickSeries { .. } => unreachable!(),
    };

    let rendered = render_report(&cli.format, &report)?;
    write_output(&cli, &rendered)?;
    remove_dir_robust(&scratch_dir());
    Ok(())
}

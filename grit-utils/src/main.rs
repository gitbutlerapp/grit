use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grit_utils::binary::{require_hyperfine, resolve_binary};
use grit_utils::compare::compare_files;
use grit_utils::fixture::{remove_dir_robust, scratch_dir};
use grit_utils::render::{render_markdown, render_text};
use grit_utils::scenarios::{run_add_suite, run_prepare_add, run_status_suite, RunConfig};
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
        /// System git used to reset the index between timed runs.
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
        let fmt = time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
            .context("build timestamp format")?;
        return OffsetDateTime::parse(ts, &fmt).context("parse --timestamp");
    }
    Ok(OffsetDateTime::now_utc())
}

fn run_config(cli: &Cli) -> RunConfig {
    RunConfig {
        warmup: cli.warmup,
        min_runs: cli.min_runs,
        prepare_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
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

    if matches!(cli.command, Cmd::PrepareAdd { .. }) {
        if let Cmd::PrepareAdd { git } = cli.command {
            let git = resolve_binary("git", Some(&git))?;
            return run_prepare_add(&git);
        }
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
    let cfg = run_config(&cli);

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
            run_status_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::Add { sizes } => {
            eprintln!("Running add benchmarks...");
            run_add_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::All { sizes } => {
            eprintln!("Running all benchmarks...");
            let mut status = run_status_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?;
            let add = run_add_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?;
            status.scenarios.extend(add.scenarios);
            status
        }
        Cmd::Compare { .. } | Cmd::PrepareAdd { .. } => unreachable!(),
    };

    let rendered = render_report(&cli.format, &report)?;
    write_output(&cli, &rendered)?;
    remove_dir_robust(&scratch_dir());
    Ok(())
}

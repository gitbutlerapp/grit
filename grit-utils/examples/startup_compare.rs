//! Focused, rotating stock/candidate/system-Git startup comparison.
//! See content/docs/benchmarking.md for build and measurement commands.
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{ensure, Context, Result};
use clap::Parser;
use grit_lib::test_support::fixtures::synth::{
    generate_synth_repo_with_spec, PathLayout, SynthTier,
};
use grit_utils::bench_env::apply_isolated_env;
use grit_utils::daily::runner::{
    run_timed_via_helper, timing_stats_from_samples, BenchArgv, RunLimits, RunPolicy,
    TimedRunSample,
};
use serde_json::json;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    stock: PathBuf,
    #[arg(long)]
    candidate: PathBuf,
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
    #[arg(long)]
    helper: PathBuf,
    #[arg(long)]
    native_helper: PathBuf,
    #[arg(long)]
    fixtures: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    profile: String,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(30..))]
    runs: u32,
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    for binary in [
        &mut args.stock,
        &mut args.candidate,
        &mut args.git,
        &mut args.helper,
        &mut args.native_helper,
    ] {
        *binary = binary
            .canonicalize()
            .with_context(|| format!("resolve {}", binary.display()))?;
    }
    std::fs::create_dir_all(&args.fixtures)?;
    args.fixtures = args.fixtures.canonicalize()?;
    let policy = RunPolicy {
        warmups_per_tool: 5,
        min_runs_per_tool: args.runs,
        max_runs_per_tool: args.runs,
        ..RunPolicy::default()
    };
    let mut rows = Vec::new();
    for (shape, files, commits, refs) in [
        ("tiny", 1, 1, 1),
        ("many-refs", 100, 100, 20_000),
        ("wide", 20_000, 100, 100),
        ("deep", 100, 20_000, 100),
    ] {
        let cwd = args.fixtures.join(shape);
        if !cwd.join(".git").exists() {
            let spec = grit_lib::test_support::fixtures::synth::TierSpec {
                file_count: files,
                commit_count: commits,
                ref_count: refs,
                edits_per_commit: 1,
                loose_after_pack: 0,
                layout: PathLayout::WideGrid { files_per_dir: 100 },
                ..SynthTier::S.spec()
            };
            generate_synth_repo_with_spec(&cwd, spec)?;
        }
        let mut fsck = Command::new(&args.git);
        fsck.current_dir(&cwd)
            .args(["fsck", "--strict", "--no-dangling"]);
        apply_isolated_env(&mut fsck);
        ensure!(
            fsck.output()?.status.success(),
            "fixture {shape} fails git fsck"
        );
        for (name, grit_argv, git_argv) in [
            (
                "branch",
                vec!["branch", "-n", "20"],
                vec!["branch", "--list"],
            ),
            ("status", vec!["status"], vec!["status", "--porcelain"]),
            (
                "resolve",
                vec!["resolve", "HEAD"],
                vec!["rev-parse", "HEAD"],
            ),
            ("help", vec!["--help"], vec!["--help"]),
            (
                "log",
                vec!["log", "-n", "1000"],
                vec!["log", "-n", "1000", "--oneline"],
            ),
        ] {
            let commands = [
                BenchArgv::new(&args.stock, grit_argv.clone()),
                BenchArgv::new(&args.candidate, grit_argv),
                BenchArgv::new(&args.git, git_argv),
            ];
            // A link-only change must preserve stdout, stderr, and exit status.
            let output = |cmd: &BenchArgv| -> Result<_> {
                let mut command = Command::new(&cmd.program);
                command
                    .args(&cmd.args)
                    .current_dir(&cwd)
                    .stdin(Stdio::null());
                apply_isolated_env(&mut command);
                command.output().context("check candidate behavior")
            };
            let stock = output(&commands[0])?;
            let candidate = output(&commands[1])?;
            ensure!(
                stock.status.success() && candidate.status.success(),
                "{shape}/{name} failed"
            );
            ensure!(
                stock.stdout == candidate.stdout && stock.stderr == candidate.stderr,
                "{shape}/{name} output changed"
            );
            let mut samples = [Vec::new(), Vec::new(), Vec::new()];
            let mut native_samples = [Vec::new(), Vec::new(), Vec::new()];
            let mut faults = [Vec::new(), Vec::new(), Vec::new()];
            for round in 0..policy.warmups_per_tool + args.runs {
                // Rotate the first tool to avoid giving one build a fixed ordering advantage.
                for offset in 0..3 {
                    let tool = (round as usize + offset) % 3;
                    let sample = run_timed_via_helper(
                        &cwd,
                        &commands[tool],
                        &RunLimits::default(),
                        &args.helper,
                    )?;
                    ensure!(
                        sample.termination == grit_utils::capped_run::TerminationKind::Success,
                        "{shape}/{name}: {:?}",
                        sample.termination
                    );
                    let mut native = Command::new(&args.native_helper);
                    native
                        .arg(&commands[tool].program)
                        .args(&commands[tool].args)
                        .current_dir(&cwd);
                    apply_isolated_env(&mut native);
                    let output = native.output()?;
                    ensure!(
                        output.status.success(),
                        "native helper failed for {shape}/{name}"
                    );
                    let text = String::from_utf8(output.stdout)?;
                    let values: Vec<_> = text.split_whitespace().collect();
                    ensure!(
                        values.len() == 4,
                        "expected wall_ms rss_kib minor_faults major_faults"
                    );
                    let native_sample = TimedRunSample {
                        wall_ms: values[0].parse()?,
                        peak_rss_bytes: values[1].parse::<u64>()? * 1024,
                        termination: grit_utils::capped_run::TerminationKind::Success,
                    };
                    if round >= policy.warmups_per_tool {
                        samples[tool].push(sample);
                        native_samples[tool].push(native_sample);
                        faults[tool].push([values[2].parse::<u64>()?, values[3].parse::<u64>()?]);
                    }
                }
            }
            let stats: Vec<_> = samples
                .iter()
                .map(|s| timing_stats_from_samples(s))
                .collect();
            eprintln!(
                "{shape}/{name}: stock {:.3}, candidate {:.3}, git {:.3} ms",
                stats[0].median_ms, stats[1].median_ms, stats[2].median_ms
            );
            let native_stats: Vec<_> = native_samples
                .iter()
                .map(|s| timing_stats_from_samples(s))
                .collect();
            rows.push(
                json!({"id": format!("startup-{}-{name}@{shape}", args.profile),
                "group": "startup-relocations", "driver": "cli",
                "description": "Warm startup via daily helper; full fork/exec/wait and RSS cross-checks in native",
                "grit": stats[1], "ratio": stats[1].median_ms / stats[2].median_ms,
                "fixture": shape, "command": name, "argv": commands.iter()
                .map(|c| json!({"program": c.program, "args": c.args})).collect::<Vec<_>>(),
                "stock": stats[0], "candidate": stats[1], "git": stats[2],
                "native": {"stock": native_stats[0], "candidate": native_stats[1], "git": native_stats[2],
                    "minor_major_faults": {"stock": faults[0], "candidate": faults[1], "git": faults[2]}}}),
            );
        }
    }
    let mut machine = grit_utils::machine::collect_machine_info(&args.fixtures)?;
    machine.cargo_profile = args.profile.clone();
    let report = json!({"schema_version": grit_utils::schema::SCHEMA_VERSION,
        "schema": "grit-startup-comparison-v1",
        "tools": {"git": grit_utils::binary::tool_version(&args.git),
            "grit": format!("{} RELR optional x86_64 GNU asset", grit_utils::binary::tool_version(&args.candidate)),
            "grit_commit": null}, "profile": args.profile,
        "timestamp": grit_utils::machine::format_timestamp(time::OffsetDateTime::now_utc()),
        "machine": machine, "git_version": grit_utils::binary::tool_version(&args.git),
        "runs_per_tool": args.runs, "warmups_per_tool": policy.warmups_per_tool,
        "method": "rotating direct argv; daily helper plus small C fork/exec/wait4 parent with per-child RSS and faults",
        "scenarios": rows});
    std::fs::write(&args.output, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

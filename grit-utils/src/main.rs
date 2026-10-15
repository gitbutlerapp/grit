use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grit_utils::binary::{require_hyperfine, resolve_binary};
use grit_utils::bitmap_suite::{run_bitmaps_suite, BitmapRunConfig};
use grit_utils::compare::compare_files;
use grit_utils::fixture::DeepLeafDogfoodSpec;
use grit_utils::fixture::{remove_dir_robust, scratch_dir};
use grit_utils::fsck_suite::run_fsck_suite;
use grit_utils::ignore_suite::run_ignore_suite;
use grit_utils::maintenance_prepare::run_prepare_maintenance;
use grit_utils::maintenance_suite::run_maintenance_suite;
use grit_utils::network_fixture::NetworkProfile;
use grit_utils::network_suite::{
    prepare_network_clone, prepare_network_fetch_incr, prepare_network_fetch_noop,
    prepare_network_push, run_network_suite,
};
use grit_utils::odb_driver::{self, open_repo};
use grit_utils::odb_suite::{run_odb_backend_suite, run_odb_suite, OdbRunConfig};
use grit_utils::parallel_fixture::ParallelProfile;
use grit_utils::parallel_suite::{
    prepare_parallel_checkout, prepare_parallel_clone, prepare_parallel_index_pack,
    run_parallel_suite,
};
use grit_utils::refs_driver;
use grit_utils::refs_fixture::{run_prepare_refs, RefsWorkMode};
use grit_utils::refs_suite::{run_refs_suite, RefsRunConfig};
use grit_utils::render::{render_markdown, render_text};
use grit_utils::revwalk_suite::run_revwalk_suite;
use grit_utils::scenarios::{
    run_add_suite, run_blame_suite, run_commit_suite, run_deep_leaf_worktree_suite, run_diff_suite,
    run_hot_path_suite, run_log_suite, run_prepare_add, run_prepare_commit, run_prepare_merge,
    run_prepare_pick, run_prepare_pick_series, run_prepare_restore, run_prepare_revert,
    run_prepare_stash, run_prepare_switch, run_restore_suite, run_stash_suite, run_status_suite,
    RunConfig,
};
use grit_utils::schema::BenchReport;
use std::net::TcpListener;
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

    #[arg(long, global = true)]
    http_server: Option<PathBuf>,

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
    /// Benchmark `grit commit` (stage all + commit) at selected repo sizes
    Commit {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000, 100_000])]
        sizes: Vec<usize>,
    },
    /// Benchmark `grit restore` at selected repo sizes
    Restore {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000, 100_000])]
        sizes: Vec<usize>,
    },
    /// Worktree diff with dozens of modified files
    Diff {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000])]
        sizes: Vec<usize>,
    },
    /// Deep-leaf CRLF worktree (dogfood #1017 layout): clean/dirty status and diff
    DeepLeaf {
        #[arg(long)]
        smoke: bool,
    },
    /// Last 100 commits, one line each
    Log {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000])]
        sizes: Vec<usize>,
    },
    /// Blame one file with deep linear history vs `git blame --porcelain`
    Blame {
        #[arg(long, default_value_t = 2_000)]
        commits: usize,
    },
    /// `grit revs --count` and `grit merge-base` vs git on deep-history and pick fixtures
    Revwalk {
        #[arg(long)]
        smoke: bool,
    },
    /// `grit fsck` vs `git fsck` on a deep-history worktree (medium fixture)
    Fsck {
        #[arg(long)]
        smoke: bool,
    },
    /// `grit gc`, repack, and commit-graph vs git on deep-history fixture
    Maintenance {
        #[arg(long)]
        smoke: bool,
    },
    /// Benchmark `grit stash` push + pop at selected repo sizes
    Stash {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000, 100_000])]
        sizes: Vec<usize>,
    },
    /// `grit resolve` / `grit cat` vs `git rev-parse` / `git cat-file -p`
    ResolveCat,
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
    /// ODB read workloads (cat-file batch, rev-list --objects, log -p) vs system git
    Odb,
    /// ODB cat-file / rev-list scenarios on the repacked 100k synthetic repo
    OdbBackend,
    /// Reachability bitmap scenarios on repacked upstream git.git
    Bitmaps {
        /// Prepared bare repo (default: cached clone at pinned tag with repack -adb)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Clone, fetch, push, and ls-remote over file and smart HTTP
    Network {
        /// Use tiny cached fixtures (for integration tests).
        #[arg(long)]
        smoke: bool,
    },
    /// Parallel scaling scenarios (threads=1 vs auto) on the 100k synthetic fixture
    Parallel {
        /// Use tiny cached fixtures (for integration tests).
        #[arg(long)]
        smoke: bool,
    },
    /// Ref workloads (for-each-ref, update-ref --stdin) on 100k-ref fixtures
    Refs,
    /// `grit ignore check` vs `git check-ignore --stdin` on 1000 paths
    Ignore,
    /// Internal: library-backed workloads for ODB benchmarks
    #[command(hide = true)]
    Drive {
        #[command(subcommand)]
        workload: DriveCmd,
    },
    /// Internal: library-backed workloads for ref benchmarks
    #[command(hide = true)]
    RefsDriver {
        #[command(subcommand)]
        workload: RefsDriveCmd,
    },
    /// Internal: measure peak RSS for one shell command (fresh process)
    #[command(hide = true)]
    MeasureRss {
        #[arg(long)]
        cwd: PathBuf,
    },
    /// Internal: run one shell command under RSS/time caps (for serve-clone hyperfine)
    #[command(hide = true)]
    RunLimited {
        #[arg(long)]
        cwd: PathBuf,
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
    #[command(hide = true)]
    PrepareRevert {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareCommit {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareMaintenance {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareRefs {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        work: PathBuf,
        #[arg(long, value_parser = parse_refs_work_mode)]
        mode: RefsWorkMode,
    },
    #[command(hide = true)]
    ServeGitHttp {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
    },
    #[command(hide = true)]
    PrepareNetworkClone {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        bare: PathBuf,
    },
    #[command(hide = true)]
    PrepareNetworkFetchIncr {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        bare: PathBuf,
    },
    #[command(hide = true)]
    PrepareNetworkFetchNoop {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        bare: PathBuf,
    },
    #[command(hide = true)]
    PrepareNetworkPush {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        bare: PathBuf,
    },
    #[command(hide = true)]
    PrepareRestore,
    #[command(hide = true)]
    PrepareStash {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareParallelCheckout {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        url: String,
        #[arg(long)]
        dest: PathBuf,
    },
    #[command(hide = true)]
    PrepareParallelClone {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        url: String,
        #[arg(long)]
        dest: PathBuf,
    },
    #[command(hide = true)]
    PrepareParallelIndexPack {
        #[arg(long)]
        pack: PathBuf,
        #[arg(long)]
        dest: PathBuf,
    },
}

fn parse_refs_work_mode(s: &str) -> Result<RefsWorkMode, String> {
    match s {
        "full" => Ok(RefsWorkMode::Full),
        "empty" => Ok(RefsWorkMode::Empty),
        other => Err(format!("expected full or empty, got {other}")),
    }
}

#[derive(Subcommand)]
enum RefsDriveCmd {
    ForEachRef {
        repo: PathBuf,
        prefix: Option<String>,
    },
    UpdateRefStdin {
        repo: PathBuf,
    },
}

#[derive(Subcommand)]
enum DriveCmd {
    CatFileBatch {
        repo: PathBuf,
    },
    CatFileBatchCheck {
        repo: PathBuf,
    },
    CatFileBatchAllUnordered {
        repo: PathBuf,
    },
    RevListObjects {
        repo: PathBuf,
    },
    LogPatch {
        repo: PathBuf,
        #[arg(default_value = "2000")]
        limit: usize,
    },
    RevListCount {
        repo: PathBuf,
    },
    RevListCountObjects {
        repo: PathBuf,
    },
    ServeClone {
        repo: PathBuf,
    },
    IndexPack {
        pack: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        threads: Option<u32>,
    },
    PackBuildAll {
        repo: PathBuf,
        #[arg(long)]
        delta_threads: Option<u32>,
    },
    DiffRenameM {
        repo: PathBuf,
        #[arg(long)]
        old: String,
        #[arg(long)]
        new: String,
    },
    CheckoutBranch {
        repo: PathBuf,
        branch: String,
    },
    CommitGraphWrite {
        repo: PathBuf,
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

fn run_refs_drive(workload: &RefsDriveCmd) -> Result<()> {
    match workload {
        RefsDriveCmd::ForEachRef { repo, prefix } => {
            let repo = refs_driver::open_repo_for_refs(repo)?;
            refs_driver::for_each_ref(&repo, prefix.as_deref())?;
        }
        RefsDriveCmd::UpdateRefStdin { repo } => {
            refs_driver::update_ref_stdin_at(repo)?;
        }
    }
    Ok(())
}

fn run_drive(workload: &DriveCmd) -> Result<()> {
    match workload {
        DriveCmd::CatFileBatch { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::cat_file_batch(&repo)?;
        }
        DriveCmd::CatFileBatchCheck { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::cat_file_batch_check(&repo)?;
        }
        DriveCmd::CatFileBatchAllUnordered { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::cat_file_batch_all_unordered(&repo)?;
        }
        DriveCmd::RevListObjects { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_objects(&repo)?;
        }
        DriveCmd::LogPatch { repo, limit } => {
            let repo = open_repo(repo)?;
            odb_driver::log_patch(&repo, *limit)?;
        }
        DriveCmd::RevListCount { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_count(&repo)?;
        }
        DriveCmd::RevListCountObjects { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_count_objects(&repo)?;
        }
        DriveCmd::ServeClone { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::serve_clone(&repo)?;
        }
        DriveCmd::IndexPack {
            pack,
            repo,
            threads,
        } => {
            let repo = open_repo(repo)?;
            odb_driver::index_pack_records(pack, &repo, threads.map(|t| t as usize))?;
        }
        DriveCmd::PackBuildAll {
            repo,
            delta_threads,
        } => {
            let repo = open_repo(repo)?;
            let threads = delta_threads.map(|t| t as usize).unwrap_or(1);
            odb_driver::pack_build_all_stdout(&repo, threads)?;
        }
        DriveCmd::DiffRenameM { repo, old, new } => {
            let repo = open_repo(repo)?;
            odb_driver::diff_rename_m(&repo, old.as_str(), new.as_str(), 50)?;
        }
        DriveCmd::CheckoutBranch { repo, branch } => {
            let repo = open_repo(repo)?;
            odb_driver::checkout_branch_force(&repo, branch.as_str())?;
        }
        DriveCmd::CommitGraphWrite { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::commit_graph_write(&repo)?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Cmd::MeasureRss { cwd } => {
            return grit_utils::resource::run_measure_rss_cli(cwd);
        }
        Cmd::RunLimited { cwd } => {
            return grit_utils::resource::run_limited_cli(cwd);
        }
        Cmd::Drive { workload } => {
            return run_drive(workload);
        }
        Cmd::RefsDriver { workload } => {
            return run_refs_drive(workload);
        }
        Cmd::PrepareRefs {
            git,
            source,
            work,
            mode,
        } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_refs(&git, source, work, *mode);
        }
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
        Cmd::PrepareRevert { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_revert(&git);
        }
        Cmd::PrepareCommit { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_commit(&git);
        }
        Cmd::PrepareMaintenance { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_maintenance(&git);
        }
        Cmd::ServeGitHttp { root, bind } => {
            let git = resolve_binary("git", cli.git.as_deref())?;
            let listener = TcpListener::bind(bind).context("bind")?;
            eprintln!(
                "git http-backend serving {} (ctrl-c to stop)",
                root.display()
            );
            for mut stream in listener.incoming().flatten() {
                let git = git.clone();
                let root = root.clone();
                std::thread::spawn(move || {
                    let _ = grit_utils::network_http::handle_git_http_connection(
                        &git,
                        &root,
                        &mut stream,
                    );
                });
            }
            return Ok(());
        }
        Cmd::PrepareNetworkClone { bare, .. } => return prepare_network_clone(bare.as_path()),
        Cmd::PrepareNetworkFetchIncr { git, bare } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_network_fetch_incr(&git, bare.as_path());
        }
        Cmd::PrepareNetworkFetchNoop { git, bare } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_network_fetch_noop(&git, bare.as_path());
        }
        Cmd::PrepareNetworkPush { git, bare } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_network_push(&git, bare.as_path());
        }
        Cmd::PrepareRestore => {
            return run_prepare_restore(&PathBuf::from("git"));
        }
        Cmd::PrepareStash { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_stash(&git);
        }
        Cmd::PrepareParallelCheckout { git, url, dest } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_parallel_checkout(&git, url, dest.as_path());
        }
        Cmd::PrepareParallelClone { git, url, dest } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_parallel_clone(&git, url, dest.as_path());
        }
        Cmd::PrepareParallelIndexPack { pack, dest } => {
            return prepare_parallel_index_pack(pack.as_path(), dest.as_path());
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
        Cmd::Commit { sizes } => {
            eprintln!("Running commit benchmarks...");
            let cfg = run_config(&cli, true);
            run_commit_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::Restore { sizes } => {
            eprintln!("Running restore benchmarks...");
            let cfg = run_config(&cli, false);
            run_restore_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::Diff { sizes } => {
            eprintln!("Running diff benchmarks...");
            let cfg = run_config(&cli, false);
            run_diff_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::DeepLeaf { smoke } => {
            eprintln!("Running deep-leaf worktree benchmarks (dogfood #1017 layout)...");
            let cfg = run_config(&cli, true);
            let spec = if *smoke {
                DeepLeafDogfoodSpec::smoke()
            } else {
                DeepLeafDogfoodSpec::production()
            };
            run_deep_leaf_worktree_suite(&hyperfine, &git, &grit, &cfg, spec, timestamp)?
        }
        Cmd::Log { sizes } => {
            eprintln!("Running log benchmarks...");
            let cfg = run_config(&cli, false);
            run_log_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::Blame { commits } => {
            eprintln!("Running blame benchmarks...");
            let cfg = run_config(&cli, true);
            run_blame_suite(&hyperfine, &git, &grit, &cfg, *commits, timestamp)?
        }
        Cmd::Revwalk { smoke } => {
            eprintln!("Running revwalk benchmarks...");
            let cfg = run_config(&cli, true);
            run_revwalk_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::Fsck { smoke } => {
            eprintln!("Running fsck benchmarks...");
            let cfg = run_config(&cli, true);
            run_fsck_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::Maintenance { smoke } => {
            eprintln!("Running maintenance (gc) benchmarks...");
            let cfg = run_config(&cli, true);
            run_maintenance_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::Stash { sizes } => {
            eprintln!("Running stash benchmarks...");
            let cfg = run_config(&cli, true);
            run_stash_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::ResolveCat => {
            eprintln!("Running resolve/cat benchmarks...");
            let cfg = run_config(&cli, true);
            grit_utils::resolve_cat_suite::run_resolve_cat_suite(
                &hyperfine, &git, &grit, &cfg, timestamp,
            )?
        }
        Cmd::Odb => {
            eprintln!("Running ODB read benchmarks...");
            let cfg = OdbRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_odb_suite(&hyperfine, &git, &grit, &bench_exe, &cfg, timestamp)?
        }
        Cmd::OdbBackend => {
            eprintln!("Running ODB backend benchmarks (100k repacked fixture)...");
            let cfg = OdbRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_odb_backend_suite(&hyperfine, &git, &grit, &bench_exe, &cfg, timestamp)?
        }
        Cmd::Bitmaps { repo } => {
            eprintln!("Running bitmap reachability benchmarks...");
            let cfg = BitmapRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_bitmaps_suite(
                &hyperfine,
                &git,
                &grit,
                &bench_exe,
                &cfg,
                repo.as_deref(),
                timestamp,
            )?
        }
        Cmd::Network { smoke } => {
            eprintln!("Running network benchmarks...");
            let profile = if *smoke {
                NetworkProfile::Smoke
            } else {
                NetworkProfile::Production
            };
            let cfg = run_config(&cli, true);
            run_network_suite(
                &hyperfine,
                &git,
                &grit,
                cli.http_server.as_deref(),
                &cfg,
                profile,
                timestamp,
            )?
        }
        Cmd::Parallel { smoke } => {
            eprintln!("Running parallel scaling benchmarks...");
            let profile = if *smoke {
                ParallelProfile::Smoke
            } else {
                ParallelProfile::Production
            };
            let cfg = run_config(&cli, true);
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_parallel_suite(
                &hyperfine, &git, &grit, &bench_exe, &cfg, profile, timestamp,
            )?
        }
        Cmd::Refs => {
            eprintln!("Running ref benchmarks (100k-ref fixtures)...");
            let cfg = RefsRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_refs_suite(&hyperfine, &git, &grit, &bench_exe, &cfg, timestamp)?
        }
        Cmd::Ignore => {
            eprintln!("Running ignore check benchmarks...");
            let cfg = run_config(&cli, false);
            run_ignore_suite(&hyperfine, &git, &grit, &cfg, timestamp)?
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
        | Cmd::MeasureRss { .. }
        | Cmd::RunLimited { .. }
        | Cmd::Drive { .. }
        | Cmd::PrepareAdd { .. }
        | Cmd::PrepareSwitch { .. }
        | Cmd::PreparePick { .. }
        | Cmd::PrepareMerge { .. }
        | Cmd::PreparePickSeries { .. }
        | Cmd::PrepareRevert { .. }
        | Cmd::PrepareCommit { .. }
        | Cmd::PrepareMaintenance { .. }
        | Cmd::PrepareRefs { .. }
        | Cmd::PrepareNetworkClone { .. }
        | Cmd::PrepareNetworkFetchIncr { .. }
        | Cmd::PrepareNetworkFetchNoop { .. }
        | Cmd::PrepareNetworkPush { .. }
        | Cmd::ServeGitHttp { .. }
        | Cmd::PrepareRestore
        | Cmd::PrepareStash { .. }
        | Cmd::PrepareParallelCheckout { .. }
        | Cmd::PrepareParallelClone { .. }
        | Cmd::PrepareParallelIndexPack { .. }
        | Cmd::RefsDriver { .. } => unreachable!(),
    };

    let rendered = render_report(&cli.format, &report)?;
    write_output(&cli, &rendered)?;
    if !matches!(
        cli.command,
        Cmd::Odb
            | Cmd::OdbBackend
            | Cmd::Refs
            | Cmd::Bitmaps { .. }
            | Cmd::Network { .. }
            | Cmd::Parallel { .. }
    ) {
        remove_dir_robust(&scratch_dir());
    }
    Ok(())
}

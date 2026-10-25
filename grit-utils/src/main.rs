use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grit_utils::binary::{require_hyperfine, resolve_binary, tool_version};
use grit_utils::bitmap_suite::{run_bitmaps_suite, BitmapRunConfig};
use grit_utils::campaign_suite::{run_campaign_suite, CampaignOptions};
use grit_utils::compare::compare_files;
use grit_utils::conflicts_suite::run_conflicts_suite;
use grit_utils::criterion_summary;
use grit_utils::fixture::{remove_dir_robust, scratch_dir};
use grit_utils::fixture::{DeepLeafDogfoodSpec, DenseLeafDogfoodSpec};
use grit_utils::fixture_registry::{self, FixtureKind};
use grit_utils::fsck_suite::run_fsck_suite;
use grit_utils::hash_suite::{prepare_hash_object, run_hash_suite, HashRunConfig};
use grit_utils::history_rewrite_prepare::run_prepare_history_rewrite;
use grit_utils::history_rewrite_suite::run_history_rewrite_suite;
use grit_utils::history_suite::run_history_suite;
use grit_utils::ignore_suite::run_ignore_suite;
use grit_utils::local_clone_suite::run_local_clone_suite;
use grit_utils::maintenance_suite::run_maintenance_suite;
use grit_utils::network_fixture::NetworkProfile;
use grit_utils::network_suite::{
    prepare_network_clone, prepare_network_fetch_incr, prepare_network_fetch_noop,
    prepare_network_force_with_lease, prepare_network_pull_rebase, prepare_network_push,
    run_network_suite,
};
use grit_utils::odb_driver::{self, open_repo};
use grit_utils::odb_suite::{run_odb_backend_suite, run_odb_suite, OdbRunConfig};
use grit_utils::parallel_fixture::ParallelProfile;
use grit_utils::parallel_suite::{
    prepare_parallel_checkout, prepare_parallel_clone, prepare_parallel_index_pack,
    run_parallel_suite,
};
use grit_utils::perf_targets::{
    ledger_failures, load_bench_report, load_perf_targets, render_ledger_markdown,
};
use grit_utils::refs_driver;
use grit_utils::refs_fixture::{run_prepare_refs, RefsWorkMode};
use grit_utils::refs_suite::{run_refs_suite, RefsRunConfig};
use grit_utils::render::{render_markdown, render_text};
use grit_utils::revision_selection_suite::run_revision_selection_suite;
use grit_utils::revwalk_suite::run_revwalk_suite;
use grit_utils::scenario_suite::{self, ScenarioSuiteOptions};
use grit_utils::scenarios::{
    run_add_suite, run_big_file_add_suite, run_blame_suite, run_commit_suite,
    run_deep_leaf_worktree_suite, run_dense_leaf_worktree_suite, run_diff_suite,
    run_hot_path_suite, run_initial_monorepo_add_suite, run_log_suite, run_merge_check_suite,
    run_merge_only_suite, run_prepare_add, run_prepare_commit,
    run_prepare_conflicts_merge_conflict_continue, run_prepare_conflicts_merge_no_ff,
    run_prepare_conflicts_pick_range, run_prepare_conflicts_revert_mainline,
    run_prepare_conflicts_revert_single, run_prepare_dense_leaf_add,
    run_prepare_initial_monorepo_add, run_prepare_merge, run_prepare_pick, run_prepare_pick_series,
    run_prepare_restore, run_prepare_revert, run_prepare_stash, run_prepare_switch,
    run_prepare_switch_detach_tag, run_prepare_switch_wide, run_restore_suite, run_stash_suite,
    run_status_suite, RunConfig,
};
use grit_utils::schema::BenchReport;
use grit_utils::synth::{generate_synth_repo, SynthReport, SynthTier};
use grit_utils::tag_tree_sha256_suite::{
    run_prepare_tag_tree_sha256_digest, run_prepare_tag_tree_sha256_signed,
    run_tag_tree_sha256_suite,
};
use grit_utils::worktree_fixture::{
    prepare_add as prepare_worktree_add, prepare_add_hunks_large as prepare_worktree_add_hunks,
    prepare_add_lines_large as prepare_worktree_add_lines,
    prepare_add_patch_file as prepare_worktree_add_patch_file,
    prepare_clean_untracked as prepare_worktree_clean, prepare_commit as prepare_worktree_commit,
    prepare_commit_staged as prepare_worktree_commit_staged, prepare_diff as prepare_worktree_diff,
    prepare_init_empty as prepare_worktree_init_empty, prepare_mv_file as prepare_worktree_mv,
    prepare_restore_paths as prepare_worktree_restore, prepare_rm_dir as prepare_worktree_rm,
    prepare_staged_diff as prepare_worktree_staged_diff,
    prepare_status_clean as prepare_worktree_status_clean,
    prepare_status_dirty as prepare_worktree_status_dirty,
};
use grit_utils::worktree_suite::run_worktree_group;
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
    /// `add` of one big untracked text file per size in MiB (#1850 layout)
    AddBigFile {
        #[arg(long, value_delimiter = ',', default_values_t = vec![256, 1_024])]
        megabytes: Vec<usize>,
    },
    /// Initial `add` on untracked monorepo-shaped tree (#1674 layout)
    AddInitial {
        #[arg(long, default_value_t = 20_000)]
        files: usize,
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
    /// Dense leaf dirs with many files per directory (dogfood #1041 layout)
    DenseLeaf {
        #[arg(long)]
        smoke: bool,
    },
    /// Last 100 commits, one line each
    Log {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000])]
        sizes: Vec<usize>,
    },
    /// Log, show, rev-list count, blame, and cat-file batch on synthetic fixtures
    History {
        /// Tier S fixture and fewer timed runs (CI integration tests).
        #[arg(long)]
        smoke: bool,
    },
    /// History rewrite ops (reword, drop, fixup, squash) vs git history / git rebase
    RewriteHistory {
        /// Smaller fixture and fewer timed runs (CI integration tests).
        #[arg(long)]
        smoke: bool,
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
    /// Revwalk filters, author log, and related revision-selection scenarios
    RevisionSelection {
        #[arg(long)]
        smoke: bool,
    },
    /// `grit fsck` vs `git fsck` on a deep-history worktree (medium fixture)
    Fsck {
        #[arg(long)]
        smoke: bool,
    },
    /// Hash-object, index-pack, and fsck baselines for checksum vs object-id work
    Hash {
        #[arg(long)]
        smoke: bool,
    },
    /// tree-sha256 tag sign/verify vs git and reference tools on real-shaped fixtures
    TagTreeSha256 {
        #[arg(long)]
        smoke: bool,
        /// Include pinned linux kernel worktree (or set `GRIT_BENCH_LINUX=1`).
        #[arg(long)]
        with_linux: bool,
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
    /// git.git / many-refs daily headline rows (hyperfine wall time + peak RSS)
    Headlines {
        #[arg(long)]
        offline: bool,
    },
    /// Switch / pick / merge hot-path scenarios (L/H sizes by default)
    HotPaths {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000, 100_000])]
        sizes: Vec<usize>,
        /// Enable `core.fsmonitor` with a trivial hook v2 script in the fixture.
        #[arg(long)]
        fsmonitor_fixture: bool,
    },
    /// Isolated merge scenario (dashboard git control; no pick/revert in the same run).
    Merge {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000])]
        sizes: Vec<usize>,
    },
    /// In-memory merge check vs `git merge-tree --write-tree` (default: 10k synthetic fixture).
    MergeCheck {
        #[arg(long, value_delimiter = ',', default_values_t = vec![10_000])]
        sizes: Vec<usize>,
    },
    /// Merge / pick / revert conflict workflows (default: 1k and 10k synthetic fixtures).
    Conflicts {
        #[arg(long, value_delimiter = ',', default_values_t = vec![1_000, 10_000])]
        sizes: Vec<usize>,
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
    /// Fully repacked local `file://` clone (default 5000-file synthetic bare remote)
    LocalClone {
        #[arg(long, default_value_t = 5_000)]
        files: usize,
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
    /// Internal: library-backed workloads for history and ODB benchmarks
    #[command(hide = true, name = "lib-op")]
    LibOp {
        #[command(subcommand)]
        workload: LibOpCmd,
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
    /// Internal: measure peak RSS for one direct argv command (fresh process)
    #[command(hide = true)]
    MeasureDirectRun {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Real-repo campaign scorecard (git.git, chromium-class synthetic, optional linux)
    Campaign {
        /// Tiny fixtures and fewer scenarios (for CI and integration tests).
        #[arg(long)]
        smoke: bool,
        /// Also run the matrix on a pinned linux kernel clone (or set `GRIT_BENCH_LINUX=1`).
        #[arg(long)]
        with_linux: bool,
        /// Render the 1.1× ledger markdown table (also needs `--format markdown` or `--output`).
        #[arg(long)]
        ledger: bool,
        /// Classify an existing JSON report instead of re-running benchmarks (requires `--ledger`).
        #[arg(long, value_name = "JSON")]
        from: Option<PathBuf>,
        /// Skip the deep-history network scenario block at the end of the run.
        #[arg(long)]
        no_network: bool,
    },
    /// Run every grit-bench scenario group for a synthetic fixture tier (S / M / L).
    Scenarios {
        #[arg(long, value_enum, default_value_t = SynthTierArg::M)]
        tier: SynthTierArg,
        /// One timed hyperfine iteration per scenario (CI smoke).
        #[arg(long)]
        smoke: bool,
        /// Require fixtures to already exist under the cache (`fixtures prepare`).
        #[arg(long)]
        offline: bool,
    },
    /// Run a scenario group against a cached registry fixture.
    Run {
        #[arg(long, value_enum)]
        group: RunGroup,
        #[arg(long, value_enum)]
        fixture: FixtureKind,
        /// CI smoke: one timed run per scenario (still compares grit vs git).
        #[arg(long)]
        smoke: bool,
        /// Do not clone or generate missing cache entries (requires prior `fixtures prepare`).
        #[arg(long)]
        offline: bool,
    },
    /// Summarize Criterion `estimates.json` files into JSON for `make bench-lib`.
    CriterionSummary {
        #[arg(long, default_value = "target/criterion")]
        criterion_dir: PathBuf,
    },
    /// Render a saved grit-bench JSON report (text or markdown).
    Render {
        /// JSON report path (`make bench-scenarios` passes `--input`).
        #[arg(long, value_name = "INPUT")]
        input: PathBuf,
    },
    /// Compare two JSON reports; exit non-zero when ratios differ beyond tolerance
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
        #[arg(long, default_value = "0.10")]
        tolerance: f64,
    },
    /// Daily performance census (headline scenarios + schema validation)
    #[command(args_conflicts_with_subcommands = true)]
    Daily {
        #[command(flatten)]
        run: DailyRunArgs,
        #[command(subcommand)]
        command: Option<DailySubCmd>,
    },
    /// Prepare and list cached benchmark fixtures (git.git + synthetic tiers)
    Fixtures {
        #[command(subcommand)]
        action: FixturesCmd,
    },
    /// Deterministic synthetic repository fixtures for benchmarks
    Fixture {
        #[command(subcommand)]
        fixture: FixtureCmd,
    },
    /// Internal: hyperfine `--prepare` hook for add benchmarks
    #[command(hide = true)]
    PrepareAdd {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareDenseLeafAdd {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareInitialMonorepoAdd {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareSwitch {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareSwitchWide {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareSwitchDetachTag {
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
    PrepareConflictsMergeNoFf {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareConflictsMergeConflictContinue {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareConflictsPickRange {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareConflictsRevertSingle {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareConflictsRevertMainline {
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
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
    PrepareHistoryRewrite {
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
    TruncatePackedRefs { repo: PathBuf, keep: usize },
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
    #[command(hide = true, name = "prepare-network-pull-rebase")]
    PrepareNetworkPullRebase {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        bare: PathBuf,
    },
    #[command(hide = true)]
    PrepareNetworkForceWithLease {
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
    PrepareDiffDirty {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareDiffSynthetic {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeStatusClean {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeStatusDirty {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareCampaignStatusDirty {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeAdd {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeAddPatchFile {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeAddHunks {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeAddLines {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeCommit {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeCommitStaged {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeDiff {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeStagedDiff {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeRestorePaths {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeRmDir {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeMvFile {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeCleanUntracked {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareWorktreeInitEmpty {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareDailyReset {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(hide = true)]
    PrepareDailyGcSeed {
        #[arg(long)]
        git: PathBuf,
    },
    #[command(
        hide = true,
        about = "Call grit-lib rename_branch for daily benchmarks"
    )]
    RenameBranchLib {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        old: String,
        #[arg(long)]
        new: String,
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
    #[command(hide = true)]
    PrepareHashObject {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        repo: PathBuf,
    },
    #[command(hide = true)]
    PrepareTagTreeSha256Digest {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        grit: PathBuf,
    },
    #[command(hide = true)]
    PrepareTagTreeSha256Signed {
        #[arg(long)]
        git: PathBuf,
        #[arg(long)]
        grit: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RunGroup {
    Worktree,
}

fn parse_refs_work_mode(s: &str) -> Result<RefsWorkMode, String> {
    match s {
        "full" => Ok(RefsWorkMode::Full),
        "empty" => Ok(RefsWorkMode::Empty),
        other => Err(format!("expected full or empty, got {other}")),
    }
}

#[derive(Clone, Debug, ValueEnum)]
enum SynthTierArg {
    S,
    M,
    L,
}

impl From<SynthTierArg> for SynthTier {
    fn from(value: SynthTierArg) -> Self {
        match value {
            SynthTierArg::S => SynthTier::S,
            SynthTierArg::M => SynthTier::M,
            SynthTierArg::L => SynthTier::L,
        }
    }
}

#[derive(clap::Args)]
struct DailyRunArgs {
    /// Print executable daily scenario ids (one per line) and exit
    #[arg(
        long,
        conflicts_with_all = ["only", "area", "smoke", "output", "baseline_grit", "offline"]
    )]
    list: bool,
    /// Run only these scenario ids (comma-separated)
    #[arg(long, value_delimiter = ',')]
    only: Option<Vec<String>>,
    /// Limit to functional areas (comma-separated)
    #[arg(long, value_delimiter = ',', value_enum)]
    area: Option<Vec<grit_utils::daily::registry::Area>>,
    /// Smoke fixtures, zero warmups, one timed run per tool
    #[arg(long)]
    smoke: bool,
    /// Write JSON report to this path
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Also time this grit binary on headline rows (`baseline_grit` in the report)
    #[arg(long)]
    baseline_grit: Option<PathBuf>,
    /// Do not clone git.git (fixtures must already be cached)
    #[arg(long)]
    offline: bool,
}

#[derive(Subcommand)]
enum DailySubCmd {
    /// Parse a report and verify schema tag, ids, and peak RSS fields
    Validate {
        /// Daily census JSON report path
        report: PathBuf,
    },
}

#[derive(Subcommand)]
enum FixturesCmd {
    /// Show fixtures present under the cache directory
    List,
    /// Download or generate fixtures into the cache
    Prepare {
        /// Fail when `git.git` is missing instead of cloning from GitHub
        #[arg(long)]
        offline: bool,
        /// Restrict to these fixtures (default: all; comma-separated)
        #[arg(long, value_delimiter = ',', value_enum)]
        only: Vec<FixtureKind>,
    },
}

#[derive(Subcommand)]
enum FixtureCmd {
    /// Build a deterministic synthetic repo (grit-lib, no `git` shell).
    Synth {
        #[arg(long, value_enum)]
        tier: SynthTierArg,
        #[arg(long)]
        out: PathBuf,
    },
}

fn render_synth_report(format: &OutputFormat, report: &SynthReport) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(format!(
            "tier={:?} head={} commits={} refs={} files={} pack_uncompressed_bytes={} generation_ms={} repo={}\n",
            report.tier,
            report.head,
            report.commit_count,
            report.ref_count,
            report.file_count,
            report.pack_uncompressed_bytes,
            report.generation_ms,
            report.repo_path.display(),
        )),
        OutputFormat::Markdown => Ok(format!(
            "## Synthetic fixture\n\n| field | value |\n| --- | --- |\n| tier | `{tier:?}` |\n| head | `{head}` |\n| commits | {commits} |\n| refs | {refs} |\n| files | {files} |\n| pack_uncompressed_bytes | {pack} |\n| generation_ms | {ms} |\n| repo | `{repo}` |\n",
            tier = report.tier,
            head = report.head,
            commits = report.commit_count,
            refs = report.ref_count,
            files = report.file_count,
            pack = report.pack_uncompressed_bytes,
            ms = report.generation_ms,
            repo = report.repo_path.display(),
        )),
        OutputFormat::Json => {
            #[derive(serde::Serialize)]
            struct JsonReport<'a> {
                tier: &'static str,
                head: &'a str,
                commit_count: usize,
                ref_count: usize,
                file_count: usize,
                pack_uncompressed_bytes: u64,
                generation_ms: u64,
                repo_path: String,
            }
            let tier = match report.tier {
                SynthTier::S => "S",
                SynthTier::M => "M",
                SynthTier::L => "L",
            };
            Ok(serde_json::to_string_pretty(&JsonReport {
                tier,
                head: &report.head,
                commit_count: report.commit_count,
                ref_count: report.ref_count,
                file_count: report.file_count,
                pack_uncompressed_bytes: report.pack_uncompressed_bytes,
                generation_ms: report.generation_ms,
                repo_path: report.repo_path.display().to_string(),
            })?)
        }
    }
}

fn render_fixtures_list(
    format: &OutputFormat,
    records: &[grit_utils::schema::FixtureRecord],
) -> Result<String> {
    use grit_utils::schema::FixtureRecord;
    match format {
        OutputFormat::Text => {
            if records.is_empty() {
                return Ok(format!(
                    "No fixtures in cache ({}).\n",
                    fixture_registry::cache_root().display()
                ));
            }
            let mut out = String::from("name\ttier\thead\tobjects\trefs\tfiles\n");
            for FixtureRecord {
                name,
                tier,
                head_oid,
                object_count,
                ref_count,
                file_count,
            } in records
            {
                let tier = tier.as_deref().unwrap_or("-");
                out.push_str(&format!(
                    "{name}\t{tier}\t{head_oid}\t{object_count}\t{ref_count}\t{file_count}\n"
                ));
            }
            Ok(out)
        }
        OutputFormat::Markdown => {
            let mut out = String::from("## Cached fixtures\n\n");
            if records.is_empty() {
                out.push_str(&format!(
                    "_No fixtures under `{}`._\n",
                    fixture_registry::cache_root().display()
                ));
                return Ok(out);
            }
            out.push_str("| name | tier | head | objects | refs | files |\n| --- | --- | --- | ---: | ---: | ---: |\n");
            for FixtureRecord {
                name,
                tier,
                head_oid,
                object_count,
                ref_count,
                file_count,
            } in records
            {
                let tier = tier.as_deref().unwrap_or("-");
                out.push_str(&format!(
                    "| `{name}` | {tier} | `{head_oid}` | {object_count} | {ref_count} | {file_count} |\n"
                ));
            }
            Ok(out)
        }
        OutputFormat::Json => {
            let mut body = serde_json::to_string_pretty(records)?;
            if !body.ends_with('\n') {
                body.push('\n');
            }
            Ok(body)
        }
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
    RefQuery {
        repo: PathBuf,
        #[arg(long, default_value = "tags")]
        namespace: String,
        #[arg(long)]
        contains: Option<String>,
        #[arg(long)]
        merged: Option<String>,
    },
}

#[derive(Subcommand)]
enum LibOpCmd {
    RevListHead {
        repo: PathBuf,
    },
    RevListCountHead {
        repo: PathBuf,
    },
    RevListCountAuthorHead {
        repo: PathBuf,
        #[arg(long, default_value = ".")]
        author: String,
    },
    RevListAuthorNHead {
        repo: PathBuf,
        #[arg(long, default_value = ".")]
        author: String,
        #[arg(long, default_value = "50")]
        max_count: usize,
    },
    BlamePorcelain {
        repo: PathBuf,
        path: String,
    },
    BlamePorcelainRange {
        repo: PathBuf,
        path: String,
        range: String,
    },
    CatFileBatch {
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
    ServeBlobless {
        repo: PathBuf,
    },
    IndexPack {
        pack: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        threads: Option<u32>,
    },
    HashObjectStdin {
        #[arg(long)]
        repo: PathBuf,
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
    RevWalkCountHeadSince {
        repo: PathBuf,
        since: i64,
    },
    RevWalkLeftRightCount {
        repo: PathBuf,
        range: String,
    },
    /// In-memory merge prediction for hot-path fixture oids (`predict_merge`).
    MergePredict {
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
        exact_runs: None,
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
        RefsDriveCmd::RefQuery {
            repo,
            namespace,
            contains,
            merged,
        } => {
            use grit_lib::objects::ObjectId;
            let repo = refs_driver::open_repo_for_refs(repo)?;
            let contains = contains
                .as_deref()
                .map(ObjectId::from_hex)
                .transpose()
                .context("parse --contains")?;
            let merged = merged
                .as_deref()
                .map(ObjectId::from_hex)
                .transpose()
                .context("parse --merged")?;
            let query = refs_driver::ref_query_for_driver(namespace, contains, merged);
            refs_driver::ref_query(&repo, &query)?;
        }
    }
    Ok(())
}

fn run_lib_op(workload: &LibOpCmd) -> Result<()> {
    match workload {
        LibOpCmd::RevListHead { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_head(&repo)?;
        }
        LibOpCmd::RevListCountHead { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_count_head(&repo)?;
        }
        LibOpCmd::RevListCountAuthorHead { repo, author } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_count_author_head(&repo, author)?;
        }
        LibOpCmd::RevListAuthorNHead {
            repo,
            author,
            max_count,
        } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_list_author_n_head(&repo, author, *max_count)?;
        }
        LibOpCmd::BlamePorcelain { repo, path } => {
            let repo = open_repo(repo)?;
            odb_driver::blame_porcelain(&repo, path)?;
        }
        LibOpCmd::BlamePorcelainRange { repo, path, range } => {
            let repo = open_repo(repo)?;
            let line_range = odb_driver::parse_blame_line_range(&repo, path, range)?;
            odb_driver::blame_porcelain_with_range(&repo, path, Some(line_range))?;
        }
        LibOpCmd::CatFileBatch { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::cat_file_batch(&repo)?;
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
        DriveCmd::ServeBlobless { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::serve_blobless(&repo)?;
        }
        DriveCmd::IndexPack {
            pack,
            repo,
            threads,
        } => {
            let repo = open_repo(repo)?;
            odb_driver::index_pack_records(pack, &repo, threads.map(|t| t as usize))?;
        }
        DriveCmd::HashObjectStdin { repo } => {
            let repo = open_repo(repo)?;
            odb_driver::hash_object_stdin_write(&repo)?;
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
        DriveCmd::RevWalkCountHeadSince { repo, since } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_walk_count_head_since(&repo, *since)?;
        }
        DriveCmd::RevWalkLeftRightCount { repo, range } => {
            let repo = open_repo(repo)?;
            odb_driver::rev_walk_left_right_count(&repo, range)?;
        }
        DriveCmd::MergePredict { repo } => {
            odb_driver::merge_predict_hot_path(repo)?;
        }
    }
    Ok(())
}

/// Run merge-check benchmarks via `grit-bench drive merge-predict` (no `grit` CLI required).
fn run_merge_check_command(
    cli: &Cli,
    git: &std::path::Path,
    hyperfine: &std::path::Path,
    timestamp: time::OffsetDateTime,
) -> Result<()> {
    let Cmd::MergeCheck { sizes } = &cli.command else {
        unreachable!("run_merge_check_command called for non-merge-check command");
    };
    let bench_exe = std::env::current_exe().context("current exe")?;
    eprintln!("git:  {} ({})", git.display(), tool_version(git));
    eprintln!(
        "grit: {} ({}, lib driver)",
        bench_exe.display(),
        tool_version(&bench_exe)
    );
    eprintln!("hyperfine: {}", hyperfine.display());
    eprintln!();
    eprintln!("Running merge-check benchmarks...");
    let cfg = run_config(cli, true);
    let report = run_merge_check_suite(hyperfine, git, &bench_exe, &cfg, sizes, timestamp)?;
    let rendered = render_report(&cli.format, &report)?;
    write_output(cli, &rendered)?;
    remove_dir_robust(&scratch_dir());
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
        Cmd::MeasureDirectRun { cwd, argv } => {
            return grit_utils::daily::runner::run_measure_direct_cli(cwd, argv);
        }
        Cmd::Daily { run, command } => match command {
            Some(DailySubCmd::Validate { report }) => {
                grit_utils::validate_daily_report(report)?;
                eprintln!("daily report valid: {}", report.display());
                return Ok(());
            }
            None => {
                if run.list {
                    for id in grit_utils::daily::list_daily_scenario_ids() {
                        println!("{id}");
                    }
                    return Ok(());
                }
                let git = resolve_binary("git", cli.git.as_deref())?;
                let grit = resolve_binary("grit", cli.grit.as_deref())?;
                let measure_helper = std::env::current_exe().context("grit-bench exe path")?;
                let timestamp = parse_timestamp(&cli)?;
                let report = grit_utils::daily::run_daily(
                    grit_utils::daily::DailyRunOptions {
                        git,
                        grit,
                        baseline_grit: run.baseline_grit.clone(),
                        measure_helper,
                        smoke: run.smoke,
                        offline: run.offline,
                        only: run.only.clone(),
                        areas: run.area.clone(),
                        output: run.output.clone(),
                    },
                    timestamp,
                )?;
                if let Some(path) = &run.output {
                    eprintln!("daily report written: {}", path.display());
                } else {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
                return Ok(());
            }
        },
        Cmd::Drive { workload } => {
            return run_drive(workload);
        }
        Cmd::LibOp { workload } => {
            return run_lib_op(workload);
        }
        Cmd::RefsDriver { workload } => {
            return run_refs_drive(workload);
        }
        Cmd::Fixtures { action } => {
            let git = resolve_binary("git", cli.git.as_deref())?;
            let body = match action {
                FixturesCmd::List => {
                    render_fixtures_list(&cli.format, &fixture_registry::list_cached(&git)?)?
                }
                FixturesCmd::Prepare { offline, only } => {
                    let records = fixture_registry::prepare_all(&git, only, *offline)?;
                    render_fixtures_list(&cli.format, &records)?
                }
            };
            write_output(&cli, &body)?;
            return Ok(());
        }
        Cmd::Fixture { fixture } => {
            match fixture {
                FixtureCmd::Synth { tier, out } => {
                    let report = generate_synth_repo(out, SynthTier::from(tier.clone()))?;
                    let body = render_synth_report(&cli.format, &report)?;
                    write_output(&cli, &body)?;
                }
            }
            return Ok(());
        }
        Cmd::Render { input } => {
            let json = std::fs::read_to_string(input)
                .with_context(|| format!("read {}", input.display()))?;
            let report = BenchReport::from_json_str(&json)?;
            let rendered = render_report(&cli.format, &report)?;
            write_output(&cli, &rendered)?;
            return Ok(());
        }
        Cmd::CriterionSummary { criterion_dir } => {
            let timestamp = parse_timestamp(&cli)?;
            let report = criterion_summary::summarize_criterion_tree(criterion_dir, timestamp)?;
            let body = match cli.format {
                OutputFormat::Json => serde_json::to_string_pretty(&report)?,
                OutputFormat::Text => format!(
                    "criterion summary: {} scenarios from {}\n",
                    report.scenarios.len(),
                    criterion_dir.display()
                ),
                OutputFormat::Markdown => {
                    format!(
                        "## Criterion summary\n\n| id | group | median ms |\n| --- | --- | ---: |\n{}",
                        report
                            .scenarios
                            .iter()
                            .map(|s| format!(
                                "| `{}` | {} | {:.2} |",
                                s.id, s.group, s.grit.median_ms
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                }
            };
            write_output(&cli, &body)?;
            return Ok(());
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
        Cmd::TruncatePackedRefs { repo, keep } => {
            grit_utils::refs_fixture::truncate_packed_refs(repo, *keep)?;
            return Ok(());
        }
        Cmd::PrepareAdd { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_add(&git);
        }
        Cmd::PrepareDenseLeafAdd { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_dense_leaf_add(&git);
        }
        Cmd::PrepareInitialMonorepoAdd { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_initial_monorepo_add(&git);
        }
        Cmd::PrepareSwitch { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_switch(&git);
        }
        Cmd::PrepareSwitchWide { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_switch_wide(&git);
        }
        Cmd::PrepareSwitchDetachTag { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_switch_detach_tag(&git);
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
        Cmd::PrepareConflictsMergeNoFf { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_conflicts_merge_no_ff(&git);
        }
        Cmd::PrepareConflictsMergeConflictContinue { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_conflicts_merge_conflict_continue(&git);
        }
        Cmd::PrepareConflictsPickRange { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_conflicts_pick_range(&git);
        }
        Cmd::PrepareConflictsRevertSingle { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_conflicts_revert_single(&git);
        }
        Cmd::PrepareConflictsRevertMainline { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_conflicts_revert_mainline(&git);
        }
        Cmd::PrepareCommit { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_commit(&git);
        }
        Cmd::PrepareMaintenance { git, worktree } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::maintenance_suite::run_prepare_maintenance(
                &git,
                worktree.as_deref(),
            );
        }
        Cmd::PrepareHistoryRewrite { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_history_rewrite(&git);
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
        Cmd::PrepareNetworkForceWithLease { git, bare } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_network_force_with_lease(&git, bare.as_path());
        }
        Cmd::PrepareNetworkPullRebase { git, bare } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_network_pull_rebase(&git, bare.as_path());
        }
        Cmd::PrepareRestore => {
            return run_prepare_restore(&PathBuf::from("git"));
        }
        Cmd::PrepareStash { git } => {
            let git = resolve_binary("git", Some(git))?;
            return run_prepare_stash(&git);
        }
        Cmd::PrepareDiffDirty { git } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::scenarios::run_prepare_diff_dirty(&git);
        }
        Cmd::PrepareDiffSynthetic { git } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::scenarios::run_prepare_diff_synthetic(&git);
        }
        Cmd::PrepareWorktreeStatusClean { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_status_clean(&git);
        }
        Cmd::PrepareWorktreeStatusDirty { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_status_dirty(&git);
        }
        Cmd::PrepareCampaignStatusDirty { git } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::campaign_prep::run_prepare_campaign_status_dirty(&git);
        }
        Cmd::PrepareWorktreeAdd { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_add(&git);
        }
        Cmd::PrepareWorktreeAddPatchFile { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_add_patch_file(&git);
        }
        Cmd::PrepareWorktreeAddHunks { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_add_hunks(&git);
        }
        Cmd::PrepareWorktreeAddLines { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_add_lines(&git);
        }
        Cmd::PrepareWorktreeCommit { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_commit(&git);
        }
        Cmd::PrepareWorktreeCommitStaged { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_commit_staged(&git);
        }
        Cmd::PrepareWorktreeDiff { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_diff(&git);
        }
        Cmd::PrepareWorktreeStagedDiff { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_staged_diff(&git);
        }
        Cmd::PrepareWorktreeRestorePaths { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_restore(&git);
        }
        Cmd::PrepareWorktreeRmDir { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_rm(&git);
        }
        Cmd::PrepareWorktreeMvFile { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_mv(&git);
        }
        Cmd::PrepareWorktreeCleanUntracked { git } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_worktree_clean(&git);
        }
        Cmd::PrepareWorktreeInitEmpty { git } => {
            let _git = resolve_binary("git", Some(git))?;
            return prepare_worktree_init_empty(_git.as_path());
        }
        Cmd::PrepareDailyReset { git } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::daily::matrix_helpers::run_prepare_daily_reset(&git);
        }
        Cmd::PrepareDailyGcSeed { git } => {
            let git = resolve_binary("git", Some(git))?;
            return grit_utils::daily::matrix_helpers::run_prepare_daily_gc_seed(&git);
        }
        Cmd::RenameBranchLib { repo, old, new } => {
            use grit_lib::config::environment::RepositoryOptions;
            use grit_lib::porcelain::branch::{rename_branch, RenameBranchOptions};
            use grit_lib::repo::Repository;
            let repo = Repository::discover(Some(repo.as_path()), &RepositoryOptions::empty())?;
            rename_branch(
                &repo,
                &RenameBranchOptions {
                    old: old.clone(),
                    new: new.clone(),
                    force: false,
                    dry_run: false,
                },
            )?;
            return Ok(());
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
        Cmd::PrepareHashObject { git, repo } => {
            let git = resolve_binary("git", Some(git))?;
            return prepare_hash_object(&git, repo.as_path());
        }
        Cmd::PrepareTagTreeSha256Digest { git: _, grit } => {
            let grit = resolve_binary("grit", Some(grit))?;
            return run_prepare_tag_tree_sha256_digest(&grit);
        }
        Cmd::PrepareTagTreeSha256Signed { git: _, grit } => {
            let grit = resolve_binary("grit", Some(grit))?;
            return run_prepare_tag_tree_sha256_signed(&grit);
        }
        _ => {}
    }

    if let Cmd::Compare {
        baseline,
        candidate,
        tolerance,
    } = &cli.command
    {
        let outcome = compare_files(baseline, candidate, *tolerance)?;
        if outcome.is_success() {
            eprintln!("All scenario ratios within tolerance {tolerance}");
            eprintln!(
                "Compared {} scenario rows; skipped {} grit hyperfine failures, {} git median drift, {} grit median drift; {} missing in candidate",
                outcome.compared,
                outcome.skipped_grit_failure,
                outcome.skipped_git_median_drift,
                outcome.skipped_grit_median_drift,
                outcome.missing_in_candidate
            );
            return Ok(());
        }
        eprintln!("Ratio drift exceeds tolerance {tolerance}:");
        for m in &outcome.mismatches {
            eprintln!(
                "  {}: {:.4} vs {:.4} (delta {:.4})",
                m.scenario_id, m.ratio_a, m.ratio_b, m.delta
            );
        }
        std::process::exit(1);
    }

    if let Cmd::Campaign {
        from: Some(from_path),
        ledger: true,
        ..
    } = &cli.command
    {
        let targets_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("perf-targets.toml");
        let targets = load_perf_targets(&targets_path)?;
        let report = load_bench_report(from_path)?;
        let fails = ledger_failures(&report, &targets);
        if !fails.is_empty() {
            eprintln!(
                "Ledger: {} scenario(s) above target without documentation:",
                fails.len()
            );
            for id in &fails {
                eprintln!("  fail: {id}");
            }
        }
        let ledger_md = render_ledger_markdown(&report, &targets);
        if matches!(cli.format, OutputFormat::Markdown) {
            write_output(&cli, &ledger_md)?;
        } else if let Some(path) = &cli.output {
            let ledger_path = path.with_extension("ledger.md");
            std::fs::write(&ledger_path, &ledger_md)?;
            eprintln!("Ledger written to {}", ledger_path.display());
        } else {
            eprintln!("{ledger_md}");
        }
        if !fails.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    let git = resolve_binary("git", cli.git.as_deref())?;
    let hyperfine = require_hyperfine()?;
    let timestamp = parse_timestamp(&cli)?;

    if matches!(&cli.command, Cmd::MergeCheck { .. }) {
        return run_merge_check_command(&cli, &git, &hyperfine, timestamp);
    }

    let grit = resolve_binary("grit", cli.grit.as_deref())?;

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
        Cmd::AddBigFile { megabytes } => {
            eprintln!("Running big file add benchmarks...");
            let cfg = run_config(&cli, false);
            run_big_file_add_suite(&hyperfine, &git, &grit, &cfg, megabytes, timestamp)?
        }
        Cmd::AddInitial { files } => {
            eprintln!("Running initial monorepo add benchmark ({files} files)...");
            let cfg = run_config(&cli, false);
            run_initial_monorepo_add_suite(&hyperfine, &git, &grit, &cfg, *files, timestamp)?
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
        Cmd::DenseLeaf { smoke } => {
            eprintln!("Running dense-leaf worktree benchmarks (dogfood #1041 layout)...");
            let cfg = run_config(&cli, true);
            let spec = if *smoke {
                DenseLeafDogfoodSpec::smoke()
            } else {
                DenseLeafDogfoodSpec::production()
            };
            run_dense_leaf_worktree_suite(&hyperfine, &git, &grit, &cfg, spec, timestamp)?
        }
        Cmd::Log { sizes } => {
            eprintln!("Running log benchmarks...");
            let cfg = run_config(&cli, false);
            run_log_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
        }
        Cmd::History { smoke } => {
            eprintln!("Running history and object-inspection benchmarks...");
            let mut cfg = run_config(&cli, true);
            if *smoke {
                cfg.warmup = 0;
                cfg.exact_runs = Some(1);
            }
            let odb_cfg = OdbRunConfig {
                warmup: cfg.warmup,
                min_runs: cfg.min_runs,
                exact_runs: cfg.exact_runs,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            let bench_exe = std::env::current_exe().context("current exe")?;
            let tier = if *smoke { SynthTier::S } else { SynthTier::L };
            run_history_suite(
                &hyperfine, &git, &grit, &bench_exe, &cfg, &odb_cfg, tier, *smoke, timestamp,
            )?
        }
        Cmd::RewriteHistory { smoke } => {
            eprintln!("Running history rewrite benchmarks…");
            let mut cfg = run_config(&cli, true);
            if *smoke {
                cfg.warmup = 0;
                cfg.exact_runs = Some(1);
            }
            run_history_rewrite_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::Run {
            group,
            fixture,
            smoke,
            offline,
        } => {
            let mut cfg = run_config(&cli, true);
            if *smoke {
                cfg.warmup = 0;
                cfg.exact_runs = Some(1);
            }
            match group {
                RunGroup::Worktree => {
                    eprintln!(
                        "Running worktree scenarios on fixture {} …",
                        fixture.public_cache_name()
                    );
                    run_worktree_group(
                        &hyperfine, &git, &grit, &cfg, *fixture, *offline, timestamp,
                    )?
                }
            }
        }
        Cmd::Scenarios {
            tier,
            smoke,
            offline,
        } => {
            eprintln!("Running scenario suite (tier {tier:?}, smoke={smoke})...");
            let mut cfg = run_config(&cli, true);
            if *smoke {
                cfg.warmup = 0;
                cfg.exact_runs = Some(1);
            }
            let bench_exe = std::env::current_exe().context("current exe")?;
            scenario_suite::run_scenario_suite(
                &hyperfine,
                &git,
                &grit,
                cli.http_server.as_deref(),
                &bench_exe,
                &cfg,
                ScenarioSuiteOptions {
                    tier: SynthTier::from(tier.clone()),
                    smoke: *smoke,
                    offline: *offline,
                },
                timestamp,
            )?
        }
        Cmd::Blame { commits } => {
            eprintln!("Running blame benchmarks...");
            let cfg = run_config(&cli, true);
            run_blame_suite(&hyperfine, &git, &grit, &cfg, *commits, timestamp)?
        }
        Cmd::Revwalk { smoke } => {
            eprintln!("Running revwalk benchmarks...");
            let cfg = run_config(&cli, true);
            {
                let bench_exe = std::env::current_exe().context("current exe")?;
                run_revwalk_suite(&hyperfine, &bench_exe, &git, &grit, &cfg, *smoke, timestamp)?
            }
        }
        Cmd::RevisionSelection { smoke } => {
            eprintln!("Running revision-selection benchmarks...");
            let cfg = run_config(&cli, true);
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_revision_selection_suite(
                &hyperfine, &bench_exe, &git, &grit, &cfg, *smoke, timestamp,
            )?
        }
        Cmd::Fsck { smoke } => {
            eprintln!("Running fsck benchmarks...");
            let cfg = run_config(&cli, true);
            run_fsck_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::Hash { smoke } => {
            eprintln!("Running hash benchmarks...");
            let cfg = HashRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                exact_runs: None,
                prepare_bin: std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("grit-bench")),
            };
            run_hash_suite(&hyperfine, &git, &grit, &cfg, *smoke, timestamp)?
        }
        Cmd::TagTreeSha256 { smoke, with_linux } => {
            eprintln!("Running tree-sha256 tag benchmarks...");
            let cfg = run_config(&cli, true);
            run_tag_tree_sha256_suite(
                &hyperfine,
                &git,
                &grit,
                &cfg,
                *smoke,
                *with_linux,
                timestamp,
            )?
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
        Cmd::Headlines { offline } => {
            eprintln!("Running git.git headline benchmarks...");
            let cfg = run_config(&cli, true);
            grit_utils::headlines_suite::run_headlines_suite(
                &hyperfine, &git, &grit, &cfg, *offline, timestamp,
            )?
        }
        Cmd::Odb => {
            eprintln!("Running ODB read benchmarks...");
            let cfg = OdbRunConfig {
                warmup: cli.warmup,
                min_runs: cli.min_runs,
                exact_runs: None,
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
                exact_runs: None,
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
                exact_runs: None,
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
        Cmd::LocalClone { files } => {
            eprintln!("Running fully packed local clone benchmarks ({files} files)...");
            let cfg = run_config(&cli, true);
            let bench_exe = std::env::current_exe().context("current exe")?;
            run_local_clone_suite(&hyperfine, &git, &grit, &bench_exe, &cfg, *files, timestamp)?
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
        Cmd::Campaign {
            smoke,
            with_linux,
            ledger,
            from,
            no_network,
        } => {
            let report = if let Some(from_path) = from {
                if !*ledger {
                    anyhow::bail!("campaign --from requires --ledger");
                }
                load_bench_report(from_path)?
            } else {
                eprintln!("Running campaign benchmarks...");
                let mut cfg = run_config(&cli, true);
                if cli.warmup == 0 && cli.min_runs == 1 {
                    cfg.exact_runs = Some(1);
                }
                let bench_exe = std::env::current_exe().context("current exe")?;
                run_campaign_suite(
                    &hyperfine,
                    &git,
                    &grit,
                    &bench_exe,
                    &cfg,
                    &CampaignOptions {
                        smoke: *smoke,
                        with_linux: *with_linux,
                        include_network: !no_network,
                    },
                    timestamp,
                )?
            };
            if *ledger {
                let targets_path =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("perf-targets.toml");
                let targets = load_perf_targets(&targets_path)?;
                let fails = ledger_failures(&report, &targets);
                if !fails.is_empty() {
                    eprintln!(
                        "Ledger: {} scenario(s) above target without documentation:",
                        fails.len()
                    );
                    for id in &fails {
                        eprintln!("  fail: {id}");
                    }
                }
                let ledger_md = render_ledger_markdown(&report, &targets);
                if matches!(cli.format, OutputFormat::Markdown) {
                    write_output(&cli, &ledger_md)?;
                } else if let Some(path) = &cli.output {
                    let ledger_path = path.with_extension("ledger.md");
                    std::fs::write(&ledger_path, &ledger_md)?;
                    eprintln!("Ledger written to {}", ledger_path.display());
                } else {
                    eprintln!("{ledger_md}");
                }
                if !fails.is_empty() {
                    std::process::exit(1);
                }
            }
            report
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
        Cmd::Merge { sizes } => {
            eprintln!("Running isolated merge benchmarks...");
            let cfg = run_config(&cli, true);
            run_merge_only_suite(&hyperfine, &git, &grit, &cfg, sizes, false, timestamp)?
        }
        Cmd::Conflicts { sizes } => {
            eprintln!("Running merge/pick/revert conflict benchmarks...");
            let cfg = run_config(&cli, true);
            run_conflicts_suite(&hyperfine, &git, &grit, &cfg, sizes, timestamp)?
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
        | Cmd::Daily { .. }
        | Cmd::MeasureRss { .. }
        | Cmd::MeasureDirectRun { .. }
        | Cmd::RunLimited { .. }
        | Cmd::Drive { .. }
        | Cmd::PrepareAdd { .. }
        | Cmd::PrepareDenseLeafAdd { .. }
        | Cmd::PrepareInitialMonorepoAdd { .. }
        | Cmd::PrepareSwitch { .. }
        | Cmd::PrepareSwitchWide { .. }
        | Cmd::PrepareSwitchDetachTag { .. }
        | Cmd::PreparePick { .. }
        | Cmd::PrepareMerge { .. }
        | Cmd::PreparePickSeries { .. }
        | Cmd::PrepareRevert { .. }
        | Cmd::PrepareConflictsMergeNoFf { .. }
        | Cmd::PrepareConflictsMergeConflictContinue { .. }
        | Cmd::PrepareConflictsPickRange { .. }
        | Cmd::PrepareConflictsRevertSingle { .. }
        | Cmd::PrepareConflictsRevertMainline { .. }
        | Cmd::PrepareCommit { .. }
        | Cmd::PrepareMaintenance { .. }
        | Cmd::PrepareHistoryRewrite { .. }
        | Cmd::PrepareRefs { .. }
        | Cmd::TruncatePackedRefs { .. }
        | Cmd::PrepareNetworkClone { .. }
        | Cmd::PrepareNetworkFetchIncr { .. }
        | Cmd::PrepareNetworkFetchNoop { .. }
        | Cmd::PrepareNetworkPush { .. }
        | Cmd::PrepareNetworkForceWithLease { .. }
        | Cmd::PrepareNetworkPullRebase { .. }
        | Cmd::ServeGitHttp { .. }
        | Cmd::PrepareRestore
        | Cmd::PrepareStash { .. }
        | Cmd::PrepareDiffDirty { .. }
        | Cmd::PrepareDiffSynthetic { .. }
        | Cmd::PrepareWorktreeStatusClean { .. }
        | Cmd::PrepareWorktreeStatusDirty { .. }
        | Cmd::PrepareCampaignStatusDirty { .. }
        | Cmd::PrepareWorktreeAdd { .. }
        | Cmd::PrepareWorktreeAddPatchFile { .. }
        | Cmd::PrepareWorktreeAddHunks { .. }
        | Cmd::PrepareWorktreeAddLines { .. }
        | Cmd::PrepareWorktreeCommit { .. }
        | Cmd::PrepareWorktreeCommitStaged { .. }
        | Cmd::PrepareWorktreeDiff { .. }
        | Cmd::PrepareWorktreeStagedDiff { .. }
        | Cmd::PrepareWorktreeRestorePaths { .. }
        | Cmd::PrepareWorktreeRmDir { .. }
        | Cmd::PrepareWorktreeMvFile { .. }
        | Cmd::PrepareWorktreeCleanUntracked { .. }
        | Cmd::PrepareWorktreeInitEmpty { .. }
        | Cmd::PrepareDailyReset { .. }
        | Cmd::PrepareDailyGcSeed { .. }
        | Cmd::RenameBranchLib { .. }
        | Cmd::Render { .. }
        | Cmd::CriterionSummary { .. }
        | Cmd::PrepareParallelCheckout { .. }
        | Cmd::PrepareParallelClone { .. }
        | Cmd::PrepareParallelIndexPack { .. }
        | Cmd::PrepareHashObject { .. }
        | Cmd::PrepareTagTreeSha256Digest { .. }
        | Cmd::PrepareTagTreeSha256Signed { .. }
        | Cmd::RefsDriver { .. }
        | Cmd::LibOp { .. }
        | Cmd::Fixture { .. }
        | Cmd::Fixtures { .. }
        | Cmd::MergeCheck { .. } => unreachable!(),
    };

    let ledger_wrote_markdown = matches!(cli.command, Cmd::Campaign { ledger: true, .. })
        && matches!(cli.format, OutputFormat::Markdown)
        && cli.output.is_some();
    if !ledger_wrote_markdown {
        let rendered = render_report(&cli.format, &report)?;
        write_output(&cli, &rendered)?;
    }
    if !matches!(
        cli.command,
        Cmd::Odb
            | Cmd::OdbBackend
            | Cmd::History { .. }
            | Cmd::Refs
            | Cmd::Bitmaps { .. }
            | Cmd::Network { .. }
            | Cmd::Parallel { .. }
            | Cmd::Scenarios { .. }
    ) {
        remove_dir_robust(&scratch_dir());
    }
    Ok(())
}

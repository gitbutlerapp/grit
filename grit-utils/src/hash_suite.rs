//! Hash and checksum benchmarks vs system `git` (large blob write, index-pack, fsck).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::fsck_suite::{clone_deep_worktree, fsck_medium_scenario};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::odb_fixture::{ensure_git_git_bare, odb_isolated_env};
use crate::parallel_suite::prepare_parallel_index_pack;
use crate::resource::bench_with_peak_rss;
use crate::scenarios::{Driver, Scenario};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;

const HASH_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Hyperfine tuning for hash scenarios.
#[derive(Debug, Clone)]
pub struct HashRunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub exact_runs: Option<u32>,
    pub prepare_bin: PathBuf,
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!("{cmd} < {}", shell_quote_path(input))
}

fn find_primary_pack(repo: &Path) -> Result<PathBuf> {
    let pack_dir = repo.join("objects/pack");
    let mut packs: Vec<PathBuf> = fs::read_dir(&pack_dir)
        .with_context(|| format!("read {}", pack_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    packs.sort();
    packs
        .into_iter()
        .next()
        .with_context(|| format!("no .pack under {}", pack_dir.display()))
}

fn ensure_hash_payload() -> Result<PathBuf> {
    let path = scratch_dir().join("hash-bench-256mib.bin");
    if path.is_file() {
        let len = fs::metadata(&path)?.len();
        if len == HASH_PAYLOAD_BYTES {
            return Ok(path);
        }
        fs::remove_file(&path).ok();
    }
    fs::create_dir_all(scratch_dir()).context("create scratch dir")?;
    eprintln!(
        "Generating {} MiB random payload at {} …",
        HASH_PAYLOAD_BYTES / (1024 * 1024),
        path.display()
    );
    let mut file = fs::File::create(&path).context("create hash payload")?;
    let mut urandom = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    let mut remaining = HASH_PAYLOAD_BYTES;
    let mut buf = [0u8; 1024 * 1024];
    while remaining > 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        urandom
            .read_exact(&mut buf[..chunk])
            .context("read /dev/urandom")?;
        file.write_all(&buf[..chunk])
            .context("write hash payload")?;
        remaining -= chunk as u64;
    }
    file.sync_all().context("sync hash payload")?;
    Ok(path)
}

fn ensure_hash_object_bare(git: &Path) -> Result<PathBuf> {
    let dest = scratch_dir().join("hash-object-bench.git");
    if dest.join("HEAD").is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("hash-object bare repo path has no parent"))?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("hash-object bare repo path is not UTF-8"))?;
    let mut cmd = Command::new(git);
    cmd.args(["init", "--bare", name]).current_dir(parent);
    git_env(&mut cmd);
    let out = cmd.output().context("git init --bare for hash-object")?;
    if !out.status.success() {
        anyhow::bail!(
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(dest)
}

fn clear_loose_objects(repo: &Path) -> Result<()> {
    let objects = repo.join("objects");
    if !objects.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&objects).context("read objects dir")? {
        let entry = entry.context("objects entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()))
            || name.starts_with("tmp_")
        {
            remove_dir_robust(&entry.path());
        }
    }
    Ok(())
}

fn prepare_hash_object_command(bench_exe: &Path, git: &Path, repo: &Path) -> String {
    shell_command(
        bench_exe,
        &[
            "prepare-hash-object".into(),
            "--git".into(),
            git.display().to_string(),
            "--repo".into(),
            repo.display().to_string(),
        ],
    )
}

fn prepare_index_pack_command(bench_exe: &Path, pack: &Path, dest: &Path) -> String {
    shell_command(
        bench_exe,
        &[
            "prepare-parallel-index-pack".into(),
            "--pack".into(),
            pack.display().to_string(),
            "--dest".into(),
            dest.display().to_string(),
        ],
    )
}

struct HashScenarioRun<'a> {
    hyperfine: &'a Path,
    bench_exe: &'a Path,
    git: &'a Path,
    grit: &'a Path,
    cfg: &'a HashRunConfig,
    scenario: &'a Scenario,
    cwd: &'a Path,
    prepare: Option<&'a str>,
}

fn grit_benchmark_command(bench_exe: &Path, grit: &Path, scenario: &Scenario) -> String {
    if scenario.grit_via_shell {
        scenario.grit_argv[0].clone()
    } else if matches!(scenario.driver, Driver::Cli) {
        shell_command(grit, &scenario.grit_argv)
    } else {
        shell_command(bench_exe, &scenario.grit_argv)
    }
}

fn run_hash_scenario(run: &HashScenarioRun<'_>) -> Result<ScenarioResult> {
    let env = Some(odb_isolated_env());
    let git_cmd = if run.scenario.git_via_shell {
        run.scenario.git_argv[0].clone()
    } else {
        shell_command(run.git, &run.scenario.git_argv)
    };
    let grit_cmd = grit_benchmark_command(run.bench_exe, run.grit, run.scenario);

    let (mut git_stats, git_rss) = bench_with_peak_rss(
        run.hyperfine,
        run.bench_exe,
        &git_cmd,
        run.cwd,
        run.cfg.warmup,
        run.cfg.min_runs,
        run.cfg.exact_runs,
        "git",
        env.as_deref(),
        run.prepare,
    )?;
    let (mut grit_stats, grit_rss) = bench_with_peak_rss(
        run.hyperfine,
        run.bench_exe,
        &grit_cmd,
        run.cwd,
        run.cfg.warmup,
        run.cfg.min_runs,
        run.cfg.exact_runs,
        "grit",
        env.as_deref(),
        run.prepare,
    )?;
    git_stats.peak_rss_bytes = Some(git_rss);
    grit_stats.peak_rss_bytes = Some(grit_rss);
    let ratio = crate::stats::median_ratio(git_stats.median_ms, grit_stats.median_ms);

    Ok(ScenarioResult {
        id: run.scenario.id.clone(),
        group: run.scenario.group.clone(),
        fixture: run.scenario.fixture.clone(),
        description: run.scenario.description.clone(),
        driver: run.scenario.driver.into(),
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure: None,
    })
}

/// Prepare hook: reset loose objects before each hash-object timing.
pub fn prepare_hash_object(_git: &Path, repo: &Path) -> Result<()> {
    clear_loose_objects(repo)
}

/// End-to-end hash scenarios: 256 MiB blob write, git.git index-pack, medium fsck.
pub fn run_hash_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &HashRunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let bench_exe = std::env::current_exe().context("current exe")?;
    let payload = ensure_hash_payload()?;
    let bare = ensure_hash_object_bare(git)?;
    prepare_hash_object(git, &bare)?;

    let git_hash = shell_command(git, &["hash-object", "-w", "--stdin"]);
    let grit_hash = shell_command(
        &bench_exe,
        &[
            "drive".into(),
            "hash-object-stdin".into(),
            "--repo".into(),
            bare.display().to_string(),
        ],
    );
    let hash_prepare = prepare_hash_object_command(&bench_exe, git, &bare);
    let hash_scenario = Scenario {
        id: "hash-object-256mib".into(),
        group: "hash".into(),
        fixture: payload.display().to_string(),
        description: "Write one 256 MiB blob from stdin (hash-object -w)".into(),
        grit_argv: vec![sh_redirect_stdin(&grit_hash, &payload)],
        git_argv: vec![sh_redirect_stdin(&git_hash, &payload)],
        driver: Driver::Cli,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    };
    let hash_result = run_hash_scenario(&HashScenarioRun {
        hyperfine,
        bench_exe: &bench_exe,
        git,
        grit,
        cfg,
        scenario: &hash_scenario,
        cwd: &bare,
        prepare: Some(&hash_prepare),
    })?;

    let git_git = ensure_git_git_bare(git)?;
    let pack_path = find_primary_pack(&git_git)?;
    let staging = scratch_dir().join("hash-index-pack-staging");
    prepare_parallel_index_pack(&pack_path, &staging)?;
    let pack_in_staging = staging.join("bench.pack");
    let index_prepare = prepare_index_pack_command(&bench_exe, &pack_path, &staging);
    let git_index = shell_command(
        git,
        &[
            "index-pack",
            "--threads=0",
            &pack_in_staging.to_string_lossy(),
        ],
    );
    let grit_index = shell_command(
        &bench_exe,
        &[
            "drive".into(),
            "index-pack".into(),
            pack_in_staging.display().to_string(),
            "--repo".into(),
            git_git.display().to_string(),
            "--threads".into(),
            "0".into(),
        ],
    );
    let index_scenario = Scenario {
        id: "index-pack-git-git".into(),
        group: "hash".into(),
        fixture: git_git.display().to_string(),
        description: "Build pack index records for upstream git.git primary pack".into(),
        grit_argv: vec![grit_index],
        git_argv: vec![git_index],
        driver: Driver::Lib,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    };
    let index_result = run_hash_scenario(&HashScenarioRun {
        hyperfine,
        bench_exe: &bench_exe,
        git,
        grit,
        cfg,
        scenario: &index_scenario,
        cwd: &staging,
        prepare: Some(&index_prepare),
    })?;

    let mut scenarios = vec![hash_result, index_result];

    let fsck_profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let fsck_bare = ensure_fixture(git, fsck_profile, NetworkFixtureKind::DeepHistory)?;
    let fsck_worktree = clone_deep_worktree(git, &fsck_bare)?;
    let fsck_scenario = fsck_medium_scenario(&fsck_bare);
    let fsck_result = run_hash_scenario(&HashScenarioRun {
        hyperfine,
        bench_exe: &bench_exe,
        git,
        grit,
        cfg,
        scenario: &fsck_scenario,
        cwd: &fsck_worktree,
        prepare: None,
    })?;
    scenarios.push(fsck_result);

    let scratch = scratch_dir();
    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch)?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}

impl From<&HashRunConfig> for crate::scenarios::RunConfig {
    fn from(cfg: &HashRunConfig) -> Self {
        Self {
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            exact_runs: cfg.exact_runs,
            prepare_bin: cfg.prepare_bin.clone(),
            isolated_config: true,
        }
    }
}

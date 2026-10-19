//! `grit gc` maintenance benchmarks vs system `git`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::bench_env::isolated_env_prefix;
use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::scenarios::{run_scenario, Driver, PrepareKind, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

const GC_BASE_MARKER: &str = ".grit-bench-gc-base";

pub(crate) fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0");
}

/// Remove every entry inside `dir` but keep `dir` itself so hyperfine's cwd stays valid.
fn clear_dir_contents(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).context("read worktree dir")? {
        let entry = entry.context("worktree dir entry")?;
        remove_dir_robust(&entry.path());
    }
    Ok(())
}

fn clone_deep_worktree(git: &Path, bare: &Path) -> Result<PathBuf> {
    let dest = scratch_dir().join("gc-medium-worktree");
    fs::create_dir_all(&dest).context("create gc-medium-worktree parent")?;
    clear_dir_contents(&dest)?;
    let mut cmd = Command::new(git);
    cmd.args(["clone", bare.to_str().context("bare path utf-8")?, "."])
        .current_dir(&dest);
    git_env(&mut cmd);
    let out = cmd.output().context("git clone")?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dest)
}

pub(crate) fn seed_loose_objects(git: &Path, repo: &Path) -> Result<()> {
    let count = std::env::var("GRIT_BENCH_GC_SEED_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    for i in 0..count {
        let mut cmd = Command::new(git);
        cmd.args(["commit", "--allow-empty", "-m", &format!("gc-bench-{i}")])
            .current_dir(repo);
        git_env(&mut cmd);
        cmd.env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        let out = cmd.output().context("git commit")?;
        if !out.status.success() {
            anyhow::bail!(
                "seed commit failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

fn prepare_gc_fixture(git: &Path, bare: &Path) -> Result<PathBuf> {
    let work = clone_deep_worktree(git, bare)?;
    seed_loose_objects(git, &work)?;
    Ok(work)
}

/// Record the current `HEAD` so campaign gc prepare can `reset --hard` without recloning.
pub fn write_gc_base_marker(git: &Path, repo: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", "HEAD"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git rev-parse HEAD for gc base")?;
    if !out.status.success() {
        anyhow::bail!("rev-parse failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let oid = String::from_utf8(out.stdout).context("rev-parse stdout")?;
    fs::write(repo.join(GC_BASE_MARKER), oid.trim().as_bytes()).context("write gc base marker")?;
    Ok(())
}

fn restore_gc_worktree(git: &Path, worktree: &Path) -> Result<()> {
    let base = fs::read_to_string(worktree.join(GC_BASE_MARKER)).context("read gc base marker")?;
    let mut reset = Command::new(git);
    reset
        .args(["reset", "--hard", base.trim()])
        .current_dir(worktree);
    git_env(&mut reset);
    let out = reset.output().context("git reset --hard gc base")?;
    if !out.status.success() {
        anyhow::bail!("reset failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let mut clean = Command::new(git);
    clean.args(["clean", "-fdx"]).current_dir(worktree);
    git_env(&mut clean);
    let out = clean.output().context("git clean")?;
    if !out.status.success() {
        anyhow::bail!("clean failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    seed_loose_objects(git, worktree)?;
    fs::write(worktree.join(GC_BASE_MARKER), base.trim().as_bytes())
        .context("restore gc base marker after clean")?;
    Ok(())
}

/// Hyperfine prepare hook: restore the deep-history worktree with fresh loose objects.
pub fn run_prepare_maintenance(git: &Path, worktree: Option<&Path>) -> Result<()> {
    let bare = std::env::var("GRIT_BENCH_MAINTENANCE_BARE")
        .map(PathBuf::from)
        .context("GRIT_BENCH_MAINTENANCE_BARE must be set for prepare-maintenance")?;
    let campaign_worktree = worktree.map(PathBuf::from).or_else(|| {
        std::env::var("GRIT_BENCH_CAMPAIGN_CWD")
            .ok()
            .map(PathBuf::from)
    });
    if let Some(worktree) = campaign_worktree {
        if !worktree.join(GC_BASE_MARKER).is_file() {
            anyhow::bail!(
                "missing {} in {} (write gc base before gc scenarios)",
                GC_BASE_MARKER,
                worktree.display()
            );
        }
        return restore_gc_worktree(git, &worktree);
    }
    let _work = prepare_gc_fixture(git, &bare)?;
    Ok(())
}

/// GC, repack, and commit-graph scenarios on the deep-history worktree.
pub fn run_maintenance_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let bare = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let marker = crate::maintenance_prepare::maintenance_bare_marker_path();
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).context("create scratch dir for maintenance marker")?;
    }
    std::fs::write(&marker, bare.display().to_string()).context("write maintenance bare marker")?;
    let worktree = prepare_gc_fixture(git, &bare)?;
    write_gc_base_marker(git, &worktree)?;
    let mut scenarios = Vec::new();
    let prepare_kind = Some(PrepareKind::MaintenanceReset);

    std::env::set_var(
        "GRIT_BENCH_MAINTENANCE_BARE",
        bare.to_str().context("bare path utf-8")?,
    );

    let _ = cfg.isolated_config.then(isolated_env_prefix);

    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "gc-medium".into(),
            group: "maintenance".into(),
            fixture: bare.display().to_string(),
            description: "full grit gc vs git gc on deep-history worktree with loose objects"
                .into(),
            grit_argv: vec!["gc".into()],
            git_argv: vec!["gc".into(), "--quiet".into()],
            driver: Driver::Cli,
            prepare_kind,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &worktree,
    )?);

    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "repack-medium".into(),
            group: "maintenance".into(),
            fixture: bare.display().to_string(),
            description: "grit gc repack --bitmaps vs git repack -adb".into(),
            grit_argv: vec!["gc".into(), "repack".into(), "--bitmaps".into()],
            git_argv: vec!["repack".into(), "-adb".into()],
            driver: Driver::Cli,
            prepare_kind,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &worktree,
    )?);

    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "commit-graph-write".into(),
            group: "maintenance".into(),
            fixture: bare.display().to_string(),
            description:
                "grit gc commit-graph vs git commit-graph write --reachable --changed-paths".into(),
            grit_argv: vec!["gc".into(), "commit-graph".into()],
            git_argv: vec![
                "commit-graph".into(),
                "write".into(),
                "--reachable".into(),
                "--changed-paths".into(),
            ],
            driver: Driver::Cli,
            prepare_kind,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &worktree,
    )?);

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}

#[cfg(test)]
mod prepare_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn hyperfine_prepare_maintenance_succeeds_after_gc_base_marker() -> Result<()> {
        let git = PathBuf::from("git");
        let dir = tempfile::tempdir()?;
        let bare = dir.path().join("remote.git");
        Command::new(&git)
            .args(["init", "--bare", "-q", bare.to_str().unwrap()])
            .status()?;
        let worktree = dir.path().join("wt");
        Command::new(&git)
            .args([
                "clone",
                "-q",
                bare.to_str().unwrap(),
                worktree.to_str().unwrap(),
            ])
            .status()?;
        Command::new(&git)
            .args(["commit", "--allow-empty", "-qm", "init"])
            .current_dir(&worktree)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()?;
        write_gc_base_marker(&git, &worktree)?;
        std::env::set_var("GRIT_BENCH_MAINTENANCE_BARE", bare.to_str().unwrap());
        run_prepare_maintenance(&git, Some(worktree.as_path()))?;
        std::env::remove_var("GRIT_BENCH_MAINTENANCE_BARE");
        Ok(())
    }
}

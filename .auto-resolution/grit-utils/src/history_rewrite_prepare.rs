//! Hyperfine prepare hooks for history rewrite benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::bench_env::empty_global_config_path;
use crate::history_rewrite_fixture::{
    ensure_history_rewrite_bare, history_rewrite_work_path, load_history_rewrite_meta,
    reset_history_rewrite_work, HistoryRewriteMeta,
};
use crate::history_rewrite_scripts::write_benchmark_scripts;

pub const BARE_ENV: &str = "GRIT_BENCH_HISTORY_REWRITE_BARE";
pub const WORK_ENV: &str = "GRIT_BENCH_HISTORY_REWRITE_WORK";
pub const MODE_ENV: &str = "GRIT_BENCH_HISTORY_REWRITE_PREPARE";

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_DATE", "1700000100 +0000");
}

fn git_ok(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn stage_fixup_file(git: &Path, work: &Path, meta: &HistoryRewriteMeta) -> Result<()> {
    let path = work.join(&meta.fixup_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&path, "fixup bench staging\n").context("write fixup path")?;
    git_ok(git, work, &["add", "--", &meta.fixup_path]).context("git add fixup path")
}

/// Reset the worktree from bare and optionally stage the fixup file.
pub fn run_prepare_history_rewrite(git: &Path) -> Result<()> {
    let bare = std::env::var(BARE_ENV)
        .map(PathBuf::from)
        .context(format!("{BARE_ENV} must be set"))?;
    let work = std::env::var(WORK_ENV)
        .map(PathBuf::from)
        .context(format!("{WORK_ENV} must be set"))?;
    if let Some(parent) = work.parent() {
        std::env::set_current_dir(parent).ok();
    }
    reset_history_rewrite_work(git, &bare, &work)?;
    let meta = load_history_rewrite_meta(&bare).context("meta from bare")?;
    write_benchmark_scripts(&work, &meta)?;
    let mode = std::env::var(MODE_ENV).unwrap_or_else(|_| "default".into());
    if mode == "fixup" {
        stage_fixup_file(git, &work, &meta)?;
    }
    Ok(())
}

/// Seed env vars before running the rewrite suite.
pub fn install_history_rewrite_env(git: &Path, smoke: bool) -> Result<(PathBuf, PathBuf)> {
    let bare = ensure_history_rewrite_bare(git, smoke)?;
    let work = history_rewrite_work_path();
    if smoke {
        let work_smoke = bare
            .parent()
            .context("bare parent directory")?
            .join("history-rewrite-work-smoke");
        std::env::set_var(BARE_ENV, bare.to_str().context("bare")?);
        std::env::set_var(WORK_ENV, work_smoke.to_str().context("work")?);
        return Ok((bare.clone(), work_smoke));
    }
    std::env::set_var(BARE_ENV, bare.to_str().context("bare")?);
    std::env::set_var(WORK_ENV, work.to_str().context("work")?);
    Ok((bare, work))
}

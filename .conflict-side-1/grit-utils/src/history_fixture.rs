//! Cached synthetic repos and helper paths for history benchmark scenarios.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::{empty_global_config_path, isolated_env_prefix};
use crate::fixture::remove_dir_robust;
use crate::synth::{generate_synth_repo, SynthTier};

const READY_MARKER: &str = ".grit-bench-history-ready";
const META_FILE: &str = ".grit-bench-history-meta.json";
const HEAD_OID_LIST: &str = ".grit-bench-head-oids.txt";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HistoryMeta {
    pub head: String,
    pub show_oid: String,
    pub blame_path: String,
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_output(git: &Path, dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Scratch directory for cached history fixtures.
pub fn history_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-scratch"))
}

/// Cached worktree clone of a synthetic tier for history scenarios.
pub fn ensure_synth_history_repo(git: &Path, tier: SynthTier) -> Result<PathBuf> {
    let dest = history_cache_root().join(format!("synth-history-{}", tier_label(tier)));
    if dest.join(READY_MARKER).is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    eprintln!(
        "Generating synthetic history fixture ({}) at {} …",
        tier_label(tier),
        dest.display()
    );
    generate_synth_repo(&dest, tier).context("generate synth history fixture")?;
    write_history_meta(git, &dest)?;
    fs::write(dest.join(READY_MARKER), b"ok").context("write history ready marker")?;
    Ok(dest)
}

fn tier_label(tier: SynthTier) -> &'static str {
    match tier {
        SynthTier::S => "S",
        SynthTier::M => "M",
        SynthTier::L => "L",
    }
}

fn write_history_meta(git: &Path, repo: &Path) -> Result<()> {
    let head = git_output(git, repo, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let show_oid = git_output(git, repo, &["rev-parse", "HEAD~100"])?
        .trim()
        .to_string();
    let blame_path = pick_high_churn_path(git, repo)?;
    write_head_reachable_oid_list(git, repo)?;
    let meta = HistoryMeta {
        head,
        show_oid,
        blame_path,
    };
    let body = serde_json::to_string_pretty(&meta).context("serialize history meta")?;
    fs::write(repo.join(META_FILE), body).context("write history meta")?;
    Ok(())
}

fn pick_high_churn_path(git: &Path, repo: &Path) -> Result<String> {
    let out = git_output(
        git,
        repo,
        &["log", "--format=", "--name-only", "HEAD", "--"],
    )?;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        *counts.entry(line.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(path, _)| path)
        .ok_or_else(|| anyhow::anyhow!("no tracked paths in synth repo"))
}

/// Sorted hex object ids reachable from `HEAD` (one per line).
pub fn ensure_head_oid_list(git: &Path, repo: &Path) -> Result<PathBuf> {
    let path = repo.join(HEAD_OID_LIST);
    if !path.is_file() {
        write_head_reachable_oid_list(git, repo)?;
    }
    Ok(path)
}

fn write_head_reachable_oid_list(git: &Path, repo: &Path) -> Result<()> {
    let out = git_output(git, repo, &["rev-list", "--objects", "HEAD"])?;
    let mut oids: Vec<String> = out
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    oids.sort();
    oids.dedup();
    let body = oids.join("\n");
    fs::write(repo.join(HEAD_OID_LIST), format!("{body}\n")).context("write head oid list")?;
    Ok(())
}

/// Load cached scenario metadata written during fixture generation.
pub fn load_history_meta(repo: &Path) -> Result<HistoryMeta> {
    let raw = fs::read_to_string(repo.join(META_FILE)).context("read history meta")?;
    serde_json::from_str(&raw).context("parse history meta")
}

/// Hermetic git environment prefix for history scenarios.
pub fn history_isolated_env() -> String {
    isolated_env_prefix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn head_oid_list_is_sorted_unique() {
        let git = crate::binary::resolve_binary("git", None).expect("git");
        let dir = TempDir::new().expect("tempdir");
        generate_synth_repo(dir.path(), SynthTier::S).expect("synth");
        write_head_reachable_oid_list(&git, dir.path()).expect("oids");
        let list = fs::read_to_string(dir.path().join(HEAD_OID_LIST)).expect("read");
        let lines: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
        let mut sorted = lines.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(lines, sorted);
        assert!(!lines.is_empty());
    }
}

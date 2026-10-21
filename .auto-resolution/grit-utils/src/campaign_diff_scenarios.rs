//! Diff/log/show output benchmark scenarios for the campaign matrix.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::campaign_prep::{ensure_campaign_hot_path, load_campaign_hot_path_meta};
use crate::hot_path_fixture::HotPathMeta;
use crate::real_repo_fixture::{git_env, run_git};
use crate::scenarios::{run_scenario, Driver, RunConfig, Scenario};
use crate::schema::ScenarioResult;
use crate::shell::shell_command;

fn sid(fixture_id: &str, name: &str) -> String {
    format!("{fixture_id}/{name}")
}

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), format!("{script}; :")]
}

fn rev_parse(git: &Path, repo: &Path, rev: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", rev]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().with_context(|| format!("rev-parse {rev}"))?;
    if !out.status.success() {
        anyhow::bail!("rev-parse {rev} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn diff_range(git: &Path, repo: &Path, meta: Option<&HotPathMeta>) -> Result<(String, String)> {
    if let Some(m) = meta {
        return Ok((m.base_commit.clone(), m.main_tip.clone()));
    }
    let head = rev_parse(git, repo, "HEAD")?;
    let parent = rev_parse(git, repo, "HEAD~1")?;
    Ok((parent, head))
}

const SHOW_PATCH_CACHE: &str = ".grit-bench-show-patch-rev";

/// Commit oid with the largest `--shortstat` in a bounded scan (cached per worktree).
pub fn show_patch_target_commit(git: &Path, repo: &Path, smoke: bool) -> Result<String> {
    let cache = repo.join(SHOW_PATCH_CACHE);
    if cache.is_file() {
        let cached = std::fs::read_to_string(&cache).context("read show-patch cache")?;
        let cached = cached.trim();
        if !cached.is_empty() && rev_parse(git, repo, cached).is_ok() {
            return Ok(cached.to_owned());
        }
    }
    let limit = if smoke { 200 } else { 2000 };
    let mut cmd = Command::new(git);
    cmd.args(["rev-list", "--max-count", &limit.to_string(), "HEAD"])
        .current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("rev-list for show-patch")?;
    if !out.status.success() {
        anyhow::bail!("rev-list failed");
    }
    let mut best_oid = rev_parse(git, repo, "HEAD")?;
    let mut best_score: u64 = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let oid = line.trim();
        if oid.is_empty() {
            continue;
        }
        let mut stat_cmd = Command::new(git);
        stat_cmd
            .args(["show", "--shortstat", "--format=", oid])
            .current_dir(repo);
        git_env(&mut stat_cmd);
        if let Ok(stat_out) = stat_cmd.output() {
            if !stat_out.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&stat_out.stdout);
            let score = text
                .split_whitespace()
                .filter_map(|t| t.parse::<u64>().ok())
                .sum();
            if score > best_score {
                best_score = score;
                best_oid = oid.to_owned();
            }
        }
    }
    std::fs::write(&cache, format!("{best_oid}\n")).context("write show-patch cache")?;
    Ok(best_oid)
}

/// Append diff detail benchmark scenarios (stat, name-only, `-w`, log/show formats, log-patch CLI).
#[allow(clippy::too_many_arguments)]
pub fn append_diff_output_benchmark_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture_id: &str,
    fixture_label: &str,
    worktree: &Path,
    smoke: bool,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let meta = load_campaign_hot_path_meta(worktree).ok();
    let (rev_a, rev_b) = diff_range(git, worktree, meta.as_ref())?;
    let show_rev = show_patch_target_commit(git, worktree, smoke)?;
    let log_limit = if smoke { "20" } else { "200" };

    let rev_a_git = rev_a.clone();
    let rev_b_git = rev_b.clone();

    let rows: [(&str, &str, Vec<String>, Vec<String>); 9] = [
        (
            "diff-stat",
            "tree diff --stat between hot-path base and main tip",
            vec!["diff".into(), "--stat".into(), rev_a.clone(), rev_b.clone()],
            vec![
                "diff".into(),
                "--stat".into(),
                rev_a_git.clone(),
                rev_b_git.clone(),
            ],
        ),
        (
            "diff-name-only",
            "tree diff --name-only between base and tip",
            vec![
                "diff".into(),
                "--name-only".into(),
                rev_a.clone(),
                rev_b.clone(),
            ],
            vec![
                "diff".into(),
                "--name-only".into(),
                rev_a_git.clone(),
                rev_b_git.clone(),
            ],
        ),
        (
            "diff-w",
            "worktree diff -w (ignore all space)",
            vec!["diff".into(), "-w".into()],
            vec!["diff".into(), "-w".into()],
        ),
        (
            "diff-quiet-clean",
            "worktree diff --quiet on clean tree",
            sh_script(shell_command(grit, &["diff", "--quiet"])),
            sh_script(shell_command(git, &["diff", "--quiet"])),
        ),
        (
            "diff-quiet-dirty",
            "worktree diff --quiet with modified tracked files",
            sh_script(shell_command(grit, &["diff", "--quiet"])),
            sh_script(shell_command(git, &["diff", "--quiet"])),
        ),
        (
            "show-patch",
            "show largest shortstat commit (cached scan)",
            vec!["show".into(), show_rev.clone()],
            vec!["show".into(), show_rev],
        ),
        (
            "log-stat",
            "log --stat over recent commits",
            vec![
                "log".into(),
                "--stat".into(),
                "--limit".into(),
                log_limit.into(),
            ],
            vec!["log".into(), "--stat".into(), format!("-{log_limit}")],
        ),
        (
            "log-name-status",
            "log --name-status over recent commits",
            vec![
                "log".into(),
                "--name-status".into(),
                "--limit".into(),
                log_limit.into(),
            ],
            vec![
                "log".into(),
                "--name-status".into(),
                format!("-{log_limit}"),
            ],
        ),
        (
            "log-patch",
            "log -p over recent commits (grit log --limit, git log -p -n)",
            vec![
                "log".into(),
                "-p".into(),
                "--limit".into(),
                log_limit.into(),
            ],
            vec!["log".into(), "-p".into(), format!("-{log_limit}")],
        ),
    ];

    for (name, description, grit_argv, git_argv) in rows {
        let prepare = if name == "diff-quiet-dirty" {
            Some(crate::scenarios::PrepareKind::CampaignStatusDirty)
        } else {
            None
        };
        let scenario = Scenario {
            id: sid(fixture_id, name),
            group: if name.starts_with("log") {
                "log".into()
            } else if name.starts_with("show") {
                "show".into()
            } else {
                "diff".into()
            },
            fixture: fixture_label.to_owned(),
            description: description.into(),
            grit_argv,
            git_argv,
            driver: Driver::Cli,
            prepare_kind: prepare,
            grit_via_shell: name.starts_with("diff-quiet"),
            git_via_shell: name.starts_with("diff-quiet"),
        };
        if name == "diff-quiet-clean" {
            run_git(git, worktree, &["reset", "--hard", "HEAD"])?;
            out.push(run_scenario(
                hyperfine, git, grit, cfg, &scenario, worktree,
            )?);
        } else {
            out.push(run_scenario(
                hyperfine, git, grit, cfg, &scenario, worktree,
            )?);
        }
    }

    let _ = ensure_campaign_hot_path(git, worktree);
    Ok(())
}

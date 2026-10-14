//! One-time campaign fixture preparation (hot-path branch topology).

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::campaign_worktree::list_tracked_files;
use crate::hot_path_fixture::{meta_path, HotPathMeta};
use crate::real_repo_fixture::git_env;

/// Load hot-path metadata prepared for campaign scenarios.
pub fn load_campaign_hot_path_meta(repo: &Path) -> Result<HotPathMeta> {
    crate::hot_path_fixture::load_meta(repo)
}

fn rev_parse(git: &Path, repo: &Path, rev: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", rev]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().with_context(|| format!("rev-parse {rev}"))?;
    if !out.status.success() {
        bail!("rev-parse {rev} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_git_auth(git: &Path, repo: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(repo);
    git_env(&mut cmd);
    cmd.env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
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
    Ok(())
}

fn current_branch(git: &Path, repo: &Path) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["branch", "--show-current"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git branch --show-current")?;
    if !out.status.success() {
        return Ok("main".into());
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        Ok("main".into())
    } else {
        Ok(name)
    }
}

/// Create divergent branches and a topic commit for switch/pick/merge campaign scenarios.
pub fn ensure_campaign_hot_path(git: &Path, repo: &Path) -> Result<HotPathMeta> {
    if meta_path(repo).is_file() {
        return load_campaign_hot_path_meta(repo);
    }
    let main_branch = current_branch(git, repo)?;
    let base_commit = rev_parse(git, repo, "HEAD")?;
    let files = list_tracked_files(git, repo)?;
    anyhow::ensure!(!files.is_empty(), "hot-path prep needs tracked files");
    let touch = (files.len() / 50).max(1).min(files.len());

    run_git_auth(git, repo, &["branch", "-f", "campaign-a", "HEAD"])?;
    for rel in files
        .iter()
        .filter(|p| !p.to_string_lossy().eq(".gitmodules"))
        .take(touch)
    {
        fs::write(repo.join(rel), b"campaign-a\n")?;
    }
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "campaign-a"])?;

    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;
    run_git_auth(git, repo, &["branch", "-f", "campaign-b", "HEAD"])?;
    for rel in files
        .iter()
        .filter(|p| !p.to_string_lossy().eq(".gitmodules"))
        .skip(touch)
        .take(touch)
    {
        fs::write(repo.join(rel), b"campaign-b\n")?;
    }
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "campaign-b"])?;

    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;
    run_git_auth(git, repo, &["branch", "-f", "campaign-topic", "HEAD"])?;
    for rel in files
        .iter()
        .filter(|p| !p.to_string_lossy().eq(".gitmodules"))
        .take(touch / 2 + 1)
    {
        fs::write(repo.join(rel), b"campaign-topic\n")?;
    }
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "campaign-topic"])?;
    let pick_commit = rev_parse(git, repo, "HEAD")?;
    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;

    let meta = HotPathMeta {
        branch_a: "campaign-a".into(),
        branch_b: "campaign-b".into(),
        main_branch: main_branch.clone(),
        topic_branch: "campaign-topic".into(),
        base_commit: base_commit.clone(),
        main_tip: base_commit,
        pick_commit,
        pick_series_commits: Vec::new(),
        wide: false,
    };
    fs::write(meta_path(repo), serde_json::to_string_pretty(&meta)?)?;
    Ok(meta)
}

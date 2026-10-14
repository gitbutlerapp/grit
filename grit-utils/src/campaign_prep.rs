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

/// Reset the campaign worktree to a clean checkout of `main_branch` at `base_commit`.
pub fn restore_campaign_worktree_to_main(
    git: &Path,
    repo: &Path,
    meta: &HotPathMeta,
) -> Result<()> {
    run_git_auth(git, repo, &["checkout", "-f", &meta.main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &meta.base_commit])?;
    run_git_auth(
        git,
        repo,
        &["clean", "-fd", "-e", ".grit-bench-hot-path.json"],
    )?;
    Ok(())
}

fn campaign_hot_path_meta_stale(git: &Path, repo: &Path, meta: &HotPathMeta) -> Result<bool> {
    let tip_a = rev_parse(git, repo, &meta.branch_a)?;
    let tip_b = rev_parse(git, repo, &meta.branch_b)?;
    let topic = rev_parse(git, repo, &meta.topic_branch)?;
    Ok(tip_a == meta.base_commit
        || tip_b == meta.base_commit
        || topic == meta.base_commit
        || tip_a == tip_b
        || tip_a == topic
        || tip_b == topic)
}

/// Create divergent branches and a topic commit for switch/pick/merge campaign scenarios.
pub fn ensure_campaign_hot_path(git: &Path, repo: &Path) -> Result<HotPathMeta> {
    if meta_path(repo).is_file() {
        let meta = load_campaign_hot_path_meta(repo)?;
        if !campaign_hot_path_meta_stale(git, repo, &meta)? {
            restore_campaign_worktree_to_main(git, repo, &meta)?;
            return Ok(meta);
        }
        fs::remove_file(meta_path(repo)).context("remove stale hot-path meta")?;
    }
    let main_branch = current_branch(git, repo)?;
    let base_commit = rev_parse(git, repo, "HEAD")?;
    let files = list_tracked_files(git, repo)?;
    anyhow::ensure!(!files.is_empty(), "hot-path prep needs tracked files");
    let touch = (files.len() / 50).max(1).min(files.len());

    run_git_auth(git, repo, &["checkout", "-B", "campaign-a"])?;
    for rel in files
        .iter()
        .filter(|p| !p.to_string_lossy().eq(".gitmodules"))
        .take(touch)
    {
        fs::write(repo.join(rel), b"campaign-a\n")?;
    }
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "campaign-a"])?;
    let tip_a = rev_parse(git, repo, "HEAD")?;

    run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;
    run_git_auth(git, repo, &["checkout", "-B", "campaign-b"])?;
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
    let tip_b = rev_parse(git, repo, "HEAD")?;
    let ref_b = rev_parse(git, repo, "campaign-b")?;
    anyhow::ensure!(
        ref_b == tip_b && tip_b != base_commit,
        "campaign-b tip did not advance (ref={ref_b}, head={tip_b}, base={base_commit})"
    );

    run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;
    run_git_auth(git, repo, &["checkout", "-B", "campaign-topic"])?;
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
    run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;

    anyhow::ensure!(tip_a != base_commit && tip_b != base_commit && pick_commit != base_commit);
    anyhow::ensure!(tip_a != tip_b && tip_a != pick_commit && tip_b != pick_commit);

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
    restore_campaign_worktree_to_main(git, repo, &meta)?;
    fs::write(meta_path(repo), serde_json::to_string_pretty(&meta)?)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::resolve_binary;
    use crate::real_repo_fixture::run_git;
    use tempfile::TempDir;

    fn init_repo_with_files(git: &Path, dir: &Path, n: usize) -> Result<()> {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        fs::create_dir_all(dir)?;
        run_git(git, dir, &["init", "-q", "-b", "campaign-main"])?;
        for i in 0..n {
            fs::write(dir.join(format!("f{i}.txt")), format!("v{i}\n"))?;
        }
        run_git(git, dir, &["add", "-A"])?;
        run_git(git, dir, &["commit", "-qm", "initial"])?;
        Ok(())
    }

    #[test]
    fn hot_path_branches_have_distinct_tips() -> Result<()> {
        let git = resolve_binary("git", None)?;
        let tmp = TempDir::new()?;
        let dir = tmp.path().join("repo");
        init_repo_with_files(&git, &dir, 120)?;
        let meta = ensure_campaign_hot_path(&git, &dir)?;
        let base = rev_parse(&git, &dir, &meta.base_commit)?;
        let head = rev_parse(&git, &dir, "HEAD")?;
        let tip_a = rev_parse(&git, &dir, &meta.branch_a)?;
        assert_eq!(head, base, "main branch should sit on base after prep");
        let tip_b = rev_parse(&git, &dir, &meta.branch_b)?;
        let topic = rev_parse(&git, &dir, &meta.topic_branch)?;
        assert_ne!(tip_a, base);
        assert_ne!(tip_b, base);
        assert_ne!(topic, base);
        assert_ne!(tip_a, tip_b);
        assert_ne!(tip_a, topic);
        assert_ne!(tip_b, topic);
        assert_eq!(rev_parse(&git, &dir, "HEAD")?, base);
        Ok(())
    }

    #[test]
    fn stale_hot_path_meta_is_regenerated() -> Result<()> {
        let git = resolve_binary("git", None)?;
        let tmp = TempDir::new()?;
        let dir = tmp.path().join("repo");
        init_repo_with_files(&git, &dir, 80)?;
        let meta = ensure_campaign_hot_path(&git, &dir)?;
        run_git(
            &git,
            &dir,
            &["branch", "-f", meta.branch_b.as_str(), &meta.base_commit],
        )?;
        let again = ensure_campaign_hot_path(&git, &dir)?;
        let tip_b = rev_parse(&git, &dir, &again.branch_b)?;
        assert_ne!(tip_b, again.base_commit);
        Ok(())
    }
}

//! One-time campaign fixture preparation (hot-path branch topology).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::campaign_worktree::{dirty_tracked_worktree, list_tracked_files};
use crate::conflicts_fixture::{
    conflict_overlap_for_size, conflicts_meta_path, load_conflicts_meta,
    persist_conflicts_meta_campaign, ConflictsMeta, BENCH_CONFLICTS_TOPIC,
};
use crate::hot_path_fixture::{
    assert_pick_merge_sibling_topology, commit_main_diverge, meta_path, touch_paths_for_size,
    HotPathMeta,
};
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

/// Tag used by the git-dot-git `switch-detach-tag` campaign row (issue #1158).
pub const GIT_DOT_GIT_SWITCH_DETACH_TAG: &str = "v1.7.3";

/// Hyperfine prepare: clean checkout of the main branch before alternating detach tag / main switch.
pub fn prepare_switch_detach_tag(git: &Path, repo: &Path) -> Result<()> {
    if meta_path(repo).is_file() {
        let meta = load_campaign_hot_path_meta(repo)?;
        restore_campaign_worktree_to_main(git, repo, &meta)?;
    } else {
        let main_branch = current_branch(git, repo).unwrap_or_else(|_| "master".into());
        run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
        run_git_auth(git, repo, &["reset", "--hard", "HEAD"])?;
    }
    run_git_auth(
        git,
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/tags/{GIT_DOT_GIT_SWITCH_DETACH_TAG}"),
        ],
    )
    .or_else(|_| {
        run_git_auth(
            git,
            repo,
            &["rev-parse", "--verify", GIT_DOT_GIT_SWITCH_DETACH_TAG],
        )
    })?;
    Ok(())
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
        &[
            "clean",
            "-fd",
            "-e",
            ".grit-bench-hot-path.json",
            "-e",
            ".grit-bench-worktree-ready",
        ],
    )?;
    Ok(())
}

/// Align grit's index and worktree with git after git-only hyperfine or reset hooks.
pub fn sync_grit_index_to_git(grit: &Path, repo: &Path) -> Result<()> {
    let mut cmd = Command::new(grit);
    cmd.args(["reset", "--hard", "HEAD"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("grit reset --hard after git reset")?;
    if !out.status.success() {
        bail!(
            "grit reset --hard failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Hard-reset a campaign worktree to a known baseline before the scenario matrix runs.
pub fn reset_campaign_worktree_clean(git: &Path, repo: &Path) -> Result<()> {
    if meta_path(repo).is_file() {
        if let Ok(meta) = load_campaign_hot_path_meta(repo) {
            restore_campaign_worktree_to_main(git, repo, &meta)?;
            return Ok(());
        }
    }
    run_git_auth(git, repo, &["reset", "--hard", "HEAD"])?;
    run_git_auth(
        git,
        repo,
        &[
            "clean",
            "-fd",
            "-q",
            "-e",
            ".grit-bench-hot-path.json",
            "-e",
            ".grit-bench-worktree-ready",
        ],
    )?;
    Ok(())
}

/// Hyperfine prepare: reset, apply the campaign dirty worktree shape, sync grit.
pub fn run_prepare_campaign_status_dirty(git: &Path) -> Result<()> {
    let repo =
        PathBuf::from(std::env::var("GRIT_BENCH_CAMPAIGN_CWD").context("GRIT_BENCH_CAMPAIGN_CWD")?);
    reset_campaign_worktree_clean(git, &repo)?;
    dirty_tracked_worktree(git, &repo)?;
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
        let wide_ready = meta
            .wide_branch_b
            .as_ref()
            .is_some_and(|b| rev_parse(git, repo, b).is_ok());
        if !campaign_hot_path_meta_stale(git, repo, &meta)? && wide_ready {
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

    let wide_branch_b =
        ensure_campaign_wide_branch_b(git, repo, &main_branch, &base_commit, &files)?;
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
        wide_branch_b: Some(wide_branch_b),
    };
    restore_campaign_worktree_to_main(git, repo, &meta)?;
    fs::write(meta_path(repo), serde_json::to_string_pretty(&meta)?)?;
    Ok(meta)
}

const WIDE_BRANCH_B: &str = "campaign-b-wide";

fn ensure_campaign_wide_branch_b(
    git: &Path,
    repo: &Path,
    main_branch: &str,
    base_commit: &str,
    files: &[std::path::PathBuf],
) -> Result<String> {
    let change_count = (files.len() / 10).max(1);
    run_git_auth(git, repo, &["checkout", "-B", WIDE_BRANCH_B, base_commit])?;
    let mut targets: Vec<std::path::PathBuf> = files.iter().take(change_count).cloned().collect();
    let dir_cap = (change_count / 10).max(1);
    let dirs: std::collections::BTreeSet<std::path::PathBuf> = targets
        .iter()
        .filter_map(|p| {
            p.parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_path_buf())
        })
        .collect();
    for d in dirs.iter().take(dir_cap) {
        let abs = repo.join(d);
        if abs.join(".git").exists() || abs == repo {
            continue;
        }
        if abs.is_dir() {
            fs::remove_dir_all(&abs).ok();
        }
    }
    targets.retain(|p| !dirs.iter().any(|d| p.starts_with(d)));
    for rel in &targets {
        if rel.to_string_lossy().eq(".gitmodules") {
            continue;
        }
        fs::write(repo.join(rel), b"wide-b\n")?;
    }
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "campaign-b-wide"])?;
    let wide_tip = rev_parse(git, repo, "HEAD")?;
    anyhow::ensure!(
        wide_tip != base_commit,
        "wide branch must diverge from base"
    );
    run_git_auth(git, repo, &["checkout", "-f", main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", base_commit])?;
    Ok(WIDE_BRANCH_B.into())
}

fn campaign_conflicts_meta_valid(git: &Path, repo: &Path, meta: &ConflictsMeta) -> Result<bool> {
    if meta.base.pick_series_commits.len() != 20 {
        return Ok(false);
    }
    if meta.merge_commit.is_empty() || meta.conflict_path_count == 0 {
        return Ok(false);
    }
    if meta.base.topic_branch != BENCH_CONFLICTS_TOPIC {
        return Ok(false);
    }
    for rev in [
        meta.base.main_tip.as_str(),
        meta.base.pick_commit.as_str(),
        meta.merge_commit.as_str(),
    ] {
        if rev_parse(git, repo, rev).is_err() {
            return Ok(false);
        }
    }
    for rev in &meta.base.pick_series_commits {
        if rev_parse(git, repo, rev).is_err() {
            return Ok(false);
        }
    }
    Ok(assert_pick_merge_sibling_topology(git, repo, &meta.base).is_ok())
}

/// Leave the worktree on `main_branch` at `main_tip` for conflict prepare hooks.
pub fn restore_campaign_conflicts_worktree(
    git: &Path,
    repo: &Path,
    meta: &ConflictsMeta,
) -> Result<()> {
    run_git_auth(git, repo, &["checkout", "-f", &meta.base.main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &meta.base.main_tip])?;
    Ok(())
}

fn touch_files_for_bench(repo: &Path, rel_paths: &[PathBuf], suffix: &str) -> Result<()> {
    for rel in rel_paths {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{suffix}\n"))?;
    }
    Ok(())
}

/// Branch topology and metadata for campaign merge/pick/revert conflict matrix rows.
pub fn ensure_campaign_conflicts(git: &Path, repo: &Path) -> Result<ConflictsMeta> {
    if conflicts_meta_path(repo).is_file() {
        let meta = load_conflicts_meta(repo)?;
        if campaign_conflicts_meta_valid(git, repo, &meta)? {
            restore_campaign_conflicts_worktree(git, repo, &meta)?;
            return Ok(meta);
        }
        fs::remove_file(conflicts_meta_path(repo)).context("remove stale conflicts meta")?;
    }

    let main_branch = current_branch(git, repo)?;
    run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", "HEAD"])?;
    let base_commit = rev_parse(git, repo, "HEAD")?;

    let files = list_tracked_files(git, repo)?;
    anyhow::ensure!(!files.is_empty(), "conflicts prep needs tracked files");
    let file_count = files.len();
    let touch = touch_paths_for_size(file_count).min(file_count);
    let overlap = conflict_overlap_for_size(file_count).min(touch).max(1);
    let targets: Vec<PathBuf> = files.iter().take(touch).cloned().collect();

    run_git_auth(
        git,
        repo,
        &["checkout", "-B", BENCH_CONFLICTS_TOPIC, &base_commit],
    )?;
    let mut series = Vec::with_capacity(20);
    const SERIES_LEN: usize = 20;
    for i in 0..SERIES_LEN {
        let rel = &files[i % files.len()];
        touch_files_for_bench(
            repo,
            std::slice::from_ref(rel),
            &format!("conflicts-series-{i}"),
        )?;
        run_git_auth(git, repo, &["add", "-A"])?;
        run_git_auth(
            git,
            repo,
            &["commit", "-qm", &format!("conflicts series {i}")],
        )?;
        series.push(rev_parse(git, repo, "HEAD")?);
    }
    let pick_commit = series.last().cloned().unwrap_or_default();

    run_git_auth(git, repo, &["checkout", "-f", &main_branch])?;
    run_git_auth(git, repo, &["reset", "--hard", &base_commit])?;
    let conflict_targets: Vec<PathBuf> = targets.iter().take(overlap).cloned().collect();
    touch_files_for_bench(repo, &conflict_targets, "main-conflict")?;
    run_git_auth(git, repo, &["add", "-A"])?;
    run_git_auth(git, repo, &["commit", "-qm", "main conflict overlap"])?;
    let main_tip = commit_main_diverge(git, repo, &files)?;

    let mut merge_cmd = Command::new(git);
    merge_cmd
        .args([
            "merge",
            "--no-ff",
            "-q",
            "--no-edit",
            BENCH_CONFLICTS_TOPIC,
        ])
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
    git_env(&mut merge_cmd);
    let merge_out = merge_cmd.output().context("git merge for merge_commit")?;
    if !merge_out.status.success() {
        run_git_auth(git, repo, &["checkout", "--theirs", "."])?;
        run_git_auth(git, repo, &["add", "-A"])?;
        run_git_auth(
            git,
            repo,
            &["commit", "--no-edit", "-qm", "bench conflicts merge"],
        )?;
    }
    let merge_commit = rev_parse(git, repo, "HEAD")?;
    anyhow::ensure!(
        merge_commit != main_tip,
        "expected merge commit after --no-ff merge"
    );
    run_git_auth(git, repo, &["reset", "--hard", &main_tip])?;

    let base = HotPathMeta {
        branch_a: main_branch.clone(),
        branch_b: BENCH_CONFLICTS_TOPIC.into(),
        main_branch,
        topic_branch: BENCH_CONFLICTS_TOPIC.into(),
        base_commit,
        main_tip,
        pick_commit,
        pick_series_commits: series,
        wide: false,
        wide_branch_b: None,
    };
    assert_pick_merge_sibling_topology(git, repo, &base)?;
    let meta = ConflictsMeta {
        base,
        merge_commit,
        conflict_path_count: overlap,
    };
    persist_conflicts_meta_campaign(repo, &meta)?;
    restore_campaign_conflicts_worktree(git, repo, &meta)?;
    Ok(meta)
}

/// Metadata for the campaign `switch-wide` row (~10% path delta on `wide_branch_b`).
pub fn switch_wide_meta(base: &HotPathMeta) -> Result<HotPathMeta> {
    let branch_b = base
        .wide_branch_b
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("campaign hot-path meta missing wide_branch_b"))?;
    Ok(HotPathMeta {
        branch_b,
        wide: true,
        ..base.clone()
    })
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
    fn prepare_status_dirty_leaves_tracked_modifications() -> Result<()> {
        let git = resolve_binary("git", None)?;
        let tmp = TempDir::new()?;
        let dir = tmp.path().join("repo");
        init_repo_with_files(&git, &dir, 20)?;
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", dir.display().to_string());
        run_prepare_campaign_status_dirty(&git)?;
        let content = fs::read_to_string(dir.join("f0.txt"))?;
        assert!(content.contains("campaign bench line"));
        std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
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
    fn wide_branch_has_larger_diff_than_narrow_b() -> Result<()> {
        let git = resolve_binary("git", None)?;
        let tmp = TempDir::new()?;
        let dir = tmp.path().join("repo");
        init_repo_with_files(&git, &dir, 200)?;
        let meta = ensure_campaign_hot_path(&git, &dir)?;
        let wide = meta.wide_branch_b.as_deref().expect("wide branch name");
        let narrow = &meta.branch_b;
        let diff_wide = diff_name_only_count(&git, &dir, &meta.branch_a, wide)?;
        let diff_narrow = diff_name_only_count(&git, &dir, &meta.branch_a, narrow)?;
        assert!(
            diff_wide > diff_narrow,
            "wide diff {diff_wide} should exceed narrow {diff_narrow}"
        );
        assert!(diff_wide >= 20, "expected ~10% delta, got {diff_wide}");
        Ok(())
    }

    fn diff_name_only_count(git: &Path, repo: &Path, left: &str, right: &str) -> Result<usize> {
        let mut cmd = Command::new(git);
        cmd.args(["diff", "--name-only", left, right])
            .current_dir(repo);
        git_env(&mut cmd);
        let out = cmd.output()?;
        if !out.status.success() {
            bail!("git diff --name-only failed");
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count())
    }

    #[test]
    fn campaign_conflicts_meta_has_series_and_merge() -> Result<()> {
        let git = resolve_binary("git", None)?;
        let tmp = TempDir::new()?;
        let dir = tmp.path().join("repo");
        init_repo_with_files(&git, &dir, 120)?;
        let meta = ensure_campaign_conflicts(&git, &dir)?;
        assert_eq!(meta.base.pick_series_commits.len(), 20);
        assert!(!meta.merge_commit.is_empty());
        assert!(meta.conflict_path_count > 0);
        assert_eq!(meta.base.topic_branch, BENCH_CONFLICTS_TOPIC);
        let head = rev_parse(&git, &dir, "HEAD")?;
        assert_eq!(head, meta.base.main_tip);
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

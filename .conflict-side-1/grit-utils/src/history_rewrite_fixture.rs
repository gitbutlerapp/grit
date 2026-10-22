//! Cached synthetic repo for `grit history` rewrite benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::bench_fixture::synth::{
    generate_synth_repo_with_spec, history_rewrite_bench_spec, SynthReport,
};

use crate::bench_env::empty_global_config_path;
use crate::fixture::remove_dir_robust;
use crate::history_fixture::history_cache_root;
use crate::synth::SynthTier;

const READY: &str = ".grit-bench-history-rewrite-ready";
const META: &str = ".grit-bench-history-rewrite-meta.json";
const BARE_SUFFIX: &str = "history-rewrite-bare.git";
const WORK_SUFFIX: &str = "history-rewrite-work";

/// Scenario depths and paths recorded when the fixture is built.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryRewriteMeta {
    pub commit_count: usize,
    pub reword: String,
    pub reword_rebase_line: u32,
    pub drop: String,
    pub drop_rebase_line: u32,
    pub fixup: String,
    pub fixup_path: String,
    pub squash_range: String,
    pub squash_rebase_parent: String,
    pub rebase_upstream: String,
    pub bench_message: String,
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
}

fn git_ok(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
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
    Ok(())
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

fn cache_root() -> PathBuf {
    history_cache_root()
}

pub fn history_rewrite_bare_path() -> PathBuf {
    cache_root().join(BARE_SUFFIX)
}

pub fn history_rewrite_work_path() -> PathBuf {
    cache_root().join(WORK_SUFFIX)
}

fn smoke_spec() -> grit_lib::bench_fixture::synth::TierSpec {
    let mut spec = SynthTier::S.spec();
    spec.commit_count = 200;
    spec.file_count = 500;
    spec.ref_count = 11;
    spec
}

fn build_source_repo(git: &Path, smoke: bool) -> Result<(PathBuf, SynthReport)> {
    let spec = if smoke {
        smoke_spec()
    } else {
        history_rewrite_bench_spec()
    };
    let src = cache_root().join(if smoke {
        "history-rewrite-src-smoke"
    } else {
        "history-rewrite-src-prod"
    });
    if src.join(READY).is_file() {
        let head = git_output(git, &src, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        return Ok((
            src.clone(),
            SynthReport {
                tier: spec.tier,
                head,
                commit_count: spec.commit_count,
                ref_count: spec.ref_count,
                file_count: spec.file_count,
                pack_uncompressed_bytes: 0,
                generation_ms: 0,
                repo_path: src,
            },
        ));
    }
    if src.exists() {
        remove_dir_robust(&src);
    }
    eprintln!(
        "Generating history-rewrite source fixture ({} commits, {} files)…",
        spec.commit_count, spec.file_count
    );
    let report = generate_synth_repo_with_spec(&src, spec).context("generate history rewrite")?;
    trim_and_seed_branches(git, &src, spec.commit_count, smoke)?;
    fs::write(src.join(READY), b"ok").context("write ready")?;
    Ok((src, report))
}

fn trim_and_seed_branches(git: &Path, repo: &Path, commit_count: usize, smoke: bool) -> Result<()> {
    let branch_count = if smoke { 5 } else { 50 };
    let refs = git_output(
        git,
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/heads/synth"],
    )?;
    for line in refs.lines().filter(|l| !l.is_empty()) {
        git_ok(git, repo, &["update-ref", "-d", line])?;
    }
    for tag in git_output(
        git,
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/tags"],
    )?
    .lines()
    .filter(|l| !l.is_empty())
    {
        git_ok(git, repo, &["update-ref", "-d", tag])?;
    }
    for i in 0..branch_count {
        let depth = if branch_count <= 1 {
            1
        } else {
            50 + i * (commit_count.saturating_sub(100)) / branch_count.max(1)
        };
        let spec = format!("HEAD~{depth}");
        let oid = git_output(git, repo, &["rev-parse", spec.trim()])?
            .trim()
            .to_string();
        git_ok(
            git,
            repo,
            &[
                "update-ref",
                &format!("refs/heads/bench/branch-{i:02}"),
                &oid,
            ],
        )?;
    }
    Ok(())
}

fn write_meta(_git: &Path, repo: &Path, smoke: bool) -> Result<HistoryRewriteMeta> {
    let commit_count = if smoke { 200 } else { 5_000 };
    let reword_depth = if smoke { 50 } else { 1_000 };
    let drop_depth = if smoke { 10 } else { 100 };
    let fixup_depth = if smoke { 25 } else { 500 };
    let squash_count = if smoke { 5 } else { 10 };
    let fixup_path = "bench-fixup-staging.txt".to_string();
    let bench_message = "grit-bench history rewrite message\n".to_string();
    let meta = HistoryRewriteMeta {
        commit_count,
        reword: format!("HEAD~{reword_depth}"),
        reword_rebase_line: 1,
        drop: format!("HEAD~{drop_depth}"),
        drop_rebase_line: 1,
        fixup: format!("HEAD~{fixup_depth}"),
        fixup_path: fixup_path.clone(),
        squash_range: format!("HEAD~{squash_count}..HEAD"),
        squash_rebase_parent: format!("HEAD~{squash_count}"),
        rebase_upstream: format!("HEAD~{reword_depth}"),
        bench_message,
    };
    let body = serde_json::to_string_pretty(&meta).context("serialize meta")?;
    fs::write(repo.join(META), body).context("write meta")?;
    Ok(meta)
}

/// Bare repository used to reset the mutable worktree between timed runs.
pub fn ensure_history_rewrite_bare(git: &Path, smoke: bool) -> Result<PathBuf> {
    let bare = if smoke {
        cache_root().join("history-rewrite-bare-smoke.git")
    } else {
        history_rewrite_bare_path()
    };
    if bare.join("HEAD").is_file() {
        return Ok(bare);
    }
    let (src, _report) = build_source_repo(git, smoke)?;
    if bare.exists() {
        remove_dir_robust(&bare);
    }
    git_ok(
        git,
        src.parent().unwrap_or(&src),
        &[
            "clone",
            "--bare",
            src.to_str().context("src utf-8")?,
            bare.to_str().context("bare utf-8")?,
        ],
    )?;
    write_meta(git, &src, smoke)?;
    fs::copy(src.join(META), bare.join(META)).context("copy meta to bare")?;
    Ok(bare)
}

/// Mutable worktree for hyperfine (reset from bare before each iteration).
pub fn ensure_history_rewrite_work(git: &Path, smoke: bool) -> Result<PathBuf> {
    let bare = ensure_history_rewrite_bare(git, smoke)?;
    let work = if smoke {
        cache_root().join("history-rewrite-work-smoke")
    } else {
        history_rewrite_work_path()
    };
    if !work.join(".git").exists() && !work.join("HEAD").exists() {
        reset_history_rewrite_work(git, &bare, &work)?;
    }
    Ok(work)
}

/// Replace `work` with a fresh clone of `bare`.
pub fn reset_history_rewrite_work(git: &Path, bare: &Path, work: &Path) -> Result<()> {
    if work.exists() {
        remove_dir_robust(work);
    }
    fs::create_dir_all(work.parent().unwrap_or(work)).ok();
    git_ok(
        git,
        work.parent().unwrap_or(work),
        &[
            "clone",
            bare.to_str().context("bare utf-8")?,
            work.file_name()
                .and_then(|s| s.to_str())
                .context("work name")?,
        ],
    )?;
    Ok(())
}

pub fn load_history_rewrite_meta(repo: &Path) -> Result<HistoryRewriteMeta> {
    let raw = fs::read_to_string(repo.join(META)).context("read history rewrite meta")?;
    serde_json::from_str(&raw).context("parse history rewrite meta")
}

pub fn history_rewrite_fixture_label(smoke: bool) -> String {
    if smoke {
        "history-rewrite-smoke".into()
    } else {
        "history-rewrite-prod".into()
    }
}

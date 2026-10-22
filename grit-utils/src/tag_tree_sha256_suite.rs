//! Signed tag tree-sha256 overhead on a git.git-shaped worktree (grit vs grit baseline).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::real_repo_fixture::{
    ensure_git_git_bitmaps, ensure_worktree_clone, run_git, scratch_root, DEFAULT_GIT_GIT_TAG,
};
use crate::scenarios::{run_scenario, sh_script_internal, Driver, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

fn shell_escape(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// Ensure git.git worktree with submodules and SSH tag signing configured for benchmarks.
pub fn ensure_tag_tree_sha256_worktree(git: &Path) -> Result<PathBuf> {
    let tag = pinned_git_git_tag();
    let bare = ensure_git_git_bitmaps(git)?;
    let worktree = ensure_worktree_clone(git, &bare, "tag-tree-sha256-git-git", &tag)?;
    run_git(
        git,
        &worktree,
        &["submodule", "update", "--init", "--recursive", "-q"],
    )?;

    let key = scratch_root().join("tag-bench-signing-key");
    if !key.with_extension("pub").is_file() {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .context("ssh-keygen for tag bench")?;
        if !status.success() {
            anyhow::bail!("ssh-keygen failed");
        }
    }
    let allowed = scratch_root().join("tag-bench-allowed-signers");
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).context("read signing pub")?;
    std::fs::write(&allowed, format!("bench@example.com {pub_key}")).context("write allowed")?;

    for (key, value) in [
        ("user.name", "Bench"),
        ("user.email", "bench@example.com"),
        ("gpg.format", "ssh"),
        (
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ),
        ("gpg.ssh.allowedSignersFile", &allowed.to_string_lossy()),
        ("tag.gpgSign", "false"),
    ] {
        run_git(git, &worktree, &["config", key, value])?;
    }
    Ok(worktree)
}

fn tag_sign_only_argv(grit: &Path) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag \"bench-sign-$(date +%s%N)\" --sign -m bench",
        shell_escape(grit)
    ))
}

fn tag_sign_tree_sha256_argv(grit: &Path) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag \"bench-ts-$(date +%s%N)\" --sign --tree-sha256 -m bench",
        shell_escape(grit)
    ))
}

/// Hyperfine: `grit tag --sign --tree-sha256` vs `grit tag --sign` on git.git worktree.
///
/// The scenario uses grit for both sides: `git` stats are sign-only baseline, `grit` stats are
/// sign + tree-sha256 total time.
pub fn run_tag_tree_sha256_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let worktree = ensure_tag_tree_sha256_worktree(git)?;
    let mut cfg = cfg.clone();
    if smoke {
        cfg.warmup = 1;
        cfg.min_runs = 3;
    }

    let sign_only = tag_sign_only_argv(grit);
    let with_ts = tag_sign_tree_sha256_argv(grit);

    let scenario = run_scenario(
        hyperfine,
        grit,
        grit,
        &cfg,
        &Scenario {
            id: "tag-tree-sha256-git-git".into(),
            group: "tag".into(),
            fixture: "git-dot-git".into(),
            description: "grit tag --sign --tree-sha256 vs grit tag --sign (fresh tag names)"
                .into(),
            grit_argv: with_ts,
            git_argv: sign_only,
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &worktree,
    )?;

    let hash_sample = Command::new(grit)
        .current_dir(&worktree)
        .args([
            "tag",
            "bench-hash-sample",
            "--sign",
            "--tree-sha256",
            "-m",
            "bench",
            "--json",
        ])
        .output()
        .context("sample tree-sha256 tag for hash_ms")?;
    let hash_ms = if hash_sample.status.success() {
        serde_json::from_slice::<serde_json::Value>(&hash_sample.stdout)
            .ok()
            .and_then(|v| v.get("tree_sha256_hash_ms").and_then(|n| n.as_f64()))
    } else {
        None
    };
    let _ = Command::new(git)
        .current_dir(&worktree)
        .args(["tag", "-d", "bench-hash-sample"])
        .output();

    let sign_median = scenario.git.median_ms;
    let total_median = scenario.grit.median_ms;
    let overhead_ms = total_median - sign_median;
    let hash_ms = hash_ms.unwrap_or(0.0);
    let bound_ms = hash_ms + 10.0;
    let within_bound = overhead_ms <= bound_ms;

    eprintln!(
        "tag-tree-sha256 git-git: sign-only {sign_median:.2}ms, with-ts {total_median:.2}ms, hash {hash_ms:.2}ms, overhead {overhead_ms:.2}ms (bound {bound_ms:.2}ms, ok={within_bound})"
    );

    let scratch = scratch_root();
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
        scenarios: vec![scenario],
    })
}

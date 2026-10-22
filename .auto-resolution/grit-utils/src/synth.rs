//! Re-exports and CLI-side helpers for the grit-lib synthetic fixture generator.

use std::path::Path;

use anyhow::{bail, Context, Result};

pub use grit_lib::bench_fixture::synth::{generate_synth_repo, SynthReport, SynthTier, TierSpec};

/// Validate tier **S** output using the system `git` binary (integration tests).
pub fn verify_tier_s_with_git(repo: &Path, spec: TierSpec) -> Result<()> {
    let git = crate::binary::resolve_binary("git", None)?;
    run_git_ok(&git, repo, &["fsck", "--strict", "--no-dangling"])?;
    let count = git_output(&git, repo, &["rev-list", "--count", "HEAD"])?;
    if count.trim() != spec.commit_count.to_string() {
        bail!("commit count {} != {}", count.trim(), spec.commit_count);
    }
    let refs = git_output(&git, repo, &["for-each-ref"])?;
    let ref_lines = refs.lines().filter(|l| !l.is_empty()).count();
    if ref_lines != spec.ref_count {
        bail!("ref count {ref_lines} != {}", spec.ref_count);
    }
    let status = git_output(&git, repo, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!("non-clean status:\n{status}");
    }
    Ok(())
}

fn run_git_ok(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(git)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn git_output(git: &Path, dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(git)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

//! Correctness checks for network benchmark clones (system `git` fsck + ref parity).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::empty_global_config_path;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn rev_parse(git: &Path, repo: &Path, spec: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", spec]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git rev-parse {spec} in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git rev-parse {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ref_oid(git: &Path, repo: &Path, refname: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", refname]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git rev-parse {refname} in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git rev-parse {refname} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run `git fsck --strict --no-dangling` on `clone` and compare clone refs to `source_bare`.
///
/// Matches normal (non-mirror) clone semantics: every local branch and
/// `refs/remotes/origin/*` on the clone must match the corresponding ref on the bare remote,
/// and `HEAD` must match the bare default branch tip.
pub fn verify_clone_against_bare(
    git: &Path,
    source_bare: &Path,
    clone: &Path,
    default_branch: &str,
) -> Result<()> {
    let mut fsck = Command::new(git);
    fsck.args(["fsck", "--strict", "--no-dangling"])
        .current_dir(clone);
    git_env(&mut fsck);
    let out = fsck
        .output()
        .with_context(|| format!("git fsck in {}", clone.display()))?;
    if !out.status.success() {
        bail!(
            "git fsck failed on {}: {}",
            clone.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let bare_tip = ref_oid(git, source_bare, &format!("refs/heads/{default_branch}"))?;
    let clone_head = rev_parse(git, clone, "HEAD")?;
    if bare_tip != clone_head {
        bail!("HEAD mismatch: bare {default_branch} is {bare_tip}, clone HEAD is {clone_head}");
    }

    let origin_ref = format!("refs/remotes/origin/{default_branch}");
    let origin_oid = ref_oid(git, clone, &origin_ref)
        .with_context(|| format!("clone missing remote-tracking ref {origin_ref} after clone"))?;
    if origin_oid != bare_tip {
        bail!("origin/{default_branch} mismatch: bare {bare_tip} != clone {origin_oid}");
    }

    let mut cmd = Command::new(git);
    cmd.args([
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes/origin",
    ])
    .current_dir(clone);
    git_env(&mut cmd);
    let out = cmd.output().context("git for-each-ref on clone")?;
    if !out.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        let clone_oid = ref_oid(git, clone, name)?;
        let bare_ref = if let Some(branch) = name.strip_prefix("refs/remotes/origin/") {
            if branch == "HEAD" {
                continue;
            }
            format!("refs/heads/{branch}")
        } else if name.starts_with("refs/heads/") {
            name.to_string()
        } else {
            continue;
        };
        let bare_oid = ref_oid(git, source_bare, &bare_ref)?;
        if clone_oid != bare_oid {
            bail!("ref mismatch for {name}: clone {clone_oid} != bare {bare_ref} {bare_oid}");
        }
    }
    Ok(())
}

/// After fetch, the client's remote-tracking branch must match the bare remote tip.
pub fn verify_client_fetch_tip(
    git: &Path,
    source_bare: &Path,
    client: &Path,
    default_branch: &str,
) -> Result<()> {
    let bare_oid = ref_oid(git, source_bare, &format!("refs/heads/{default_branch}"))?;
    let client_oid = ref_oid(
        git,
        client,
        &format!("refs/remotes/origin/{default_branch}"),
    )?;
    if bare_oid != client_oid {
        bail!("fetch tip mismatch: bare {bare_oid} != origin/{default_branch} {client_oid}");
    }
    Ok(())
}

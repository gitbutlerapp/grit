//! Isolated Git/grit environment for benchmark commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use crate::fixture::scratch_dir;
use crate::shell::shell_quote;

static EMPTY_GLOBAL_CONFIG: OnceLock<PathBuf> = OnceLock::new();

/// Path to an empty file used as `GIT_CONFIG_GLOBAL` for benchmarks.
pub fn empty_global_config_path() -> &'static Path {
    EMPTY_GLOBAL_CONFIG.get_or_init(|| {
        let dir = scratch_dir();
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(".grit-bench-empty-config");
        if fs::write(&path, b"").is_err() {
            return PathBuf::from("/dev/null");
        }
        path
    })
}

/// Environment prefix prepended to hyperfine benchmark shell commands.
pub fn isolated_env_prefix() -> String {
    let global = shell_quote(&empty_global_config_path().to_string_lossy());
    [
        "GIT_CONFIG_NOSYSTEM=1".to_string(),
        format!("GIT_CONFIG_GLOBAL={global}"),
        "GIT_CONFIG_SYSTEM=/dev/null".to_string(),
        "GIT_AUTHOR_NAME=Bench".to_string(),
        "GIT_AUTHOR_EMAIL=b@example.com".to_string(),
        "GIT_COMMITTER_NAME=Bench".to_string(),
        "GIT_COMMITTER_EMAIL=b@example.com".to_string(),
    ]
    .join(" ")
}

/// Write a trivial fsmonitor hook (protocol v2) and return its path.
///
/// Git invokes the hook as `hook.sh 2 <token>` and expects a NUL-delimited
/// response: new token, then changed paths (`/` means everything changed).
pub fn write_fsmonitor_hook(dir: &Path) -> Result<PathBuf> {
    let hook = dir.join(".grit-bench-fsmonitor-hook.sh");
    let script = r#"#!/bin/sh
# fsmonitor hook protocol v2 (see gitglossary fsmonitor-watchman v2)
if [ "$1" = "2" ]; then
  while read -r line; do
    [ -z "$line" ] && break
  done
  printf '%s\0' 'bench-token'
  printf '%s\0' /
  exit 0
fi
exit 0
"#;
    fs::write(&hook, script).with_context(|| format!("write fsmonitor hook {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms)?;
    }
    Ok(hook)
}

/// Configure `core.fsmonitor` in the repository (local config).
pub fn enable_fsmonitor(git: &Path, repo: &Path, hook: &Path) -> Result<()> {
    let out = Command::new(git)
        .args([
            "config",
            "core.fsmonitor",
            hook.to_str().context("hook path utf8")?,
        ])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git config core.fsmonitor")?;
    if !out.status.success() {
        bail!(
            "git config core.fsmonitor failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run `git status` and verify the index contains a non-empty FSMN token.
pub fn assert_fsmonitor_index_ready(git: &Path, repo: &Path) -> Result<()> {
    let out = Command::new(git)
        .args(["status", "-s"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git status for fsmonitor")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Empty last update token") {
        bail!("fsmonitor hook did not return a token: {stderr}");
    }
    if !out.status.success() {
        bail!("git status failed: {}", stderr.trim());
    }
    let index = fs::read(repo.join(".git/index")).context("read index")?;
    if !index.windows(4).any(|w| w == b"FSMN") {
        bail!("index missing FSMN extension after fsmonitor status");
    }
    if !index
        .windows(b"bench-token".len())
        .any(|w| w == b"bench-token")
    {
        bail!("FSMN extension missing expected bench-token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn isolated_env_prefix_contains_nosystem() {
        let prefix = isolated_env_prefix();
        assert!(prefix.contains("GIT_CONFIG_NOSYSTEM=1"));
        assert!(prefix.contains("GIT_CONFIG_GLOBAL="));
    }

    #[test]
    fn fsmonitor_hook_v2_prints_token_and_root_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook = write_fsmonitor_hook(dir.path()).expect("write hook");
        let out = Command::new(&hook)
            .args(["2", "builtin:fake"])
            .stdin(Stdio::null())
            .output()
            .expect("run hook");
        assert!(out.status.success());
        let bytes = &out.stdout;
        assert!(
            bytes.starts_with(b"bench-token\0"),
            "expected NUL-terminated token first, got {:?}",
            bytes
        );
        assert!(
            bytes.windows(2).any(|w| w == b"/\0"),
            "expected root path marker, got {:?}",
            bytes
        );
    }
}

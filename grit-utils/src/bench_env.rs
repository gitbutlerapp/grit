//! Isolated Git/grit environment for benchmark commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};

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
pub fn write_fsmonitor_hook(dir: &Path) -> Result<PathBuf> {
    let hook = dir.join(".grit-bench-fsmonitor-hook.sh");
    let script = "#!/bin/sh
case \"$1\" in
capability)
  echo version=2
  echo can-recurse=1
  ;;
query)
  echo token bench-token
  echo /
  echo done
  ;;
*)
  exit 0
  ;;
esac
";
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
    let out = std::process::Command::new(git)
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
        anyhow::bail!(
            "git config core.fsmonitor failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_env_prefix_contains_nosystem() {
        let prefix = isolated_env_prefix();
        assert!(prefix.contains("GIT_CONFIG_NOSYSTEM=1"));
        assert!(prefix.contains("GIT_CONFIG_GLOBAL="));
    }
}

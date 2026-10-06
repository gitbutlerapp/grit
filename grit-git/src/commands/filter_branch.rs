//! `grit filter-branch` — rewrite branches by delegating to the system's
//! `git-filter-branch` shell script.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::process::Command;

/// Arguments for `grit filter-branch`.
#[derive(Debug, ClapArgs)]
#[command(about = "Rewrite branches (delegates to system git-filter-branch)")]
pub struct Args {
    /// Raw arguments forwarded to git-filter-branch.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Locate a real, non-grit `git` binary suitable for querying `--exec-path`.
///
/// In the test harness plain `git` on `PATH` resolves to grit itself, so we only
/// probe absolute paths to the system git: the macOS Xcode/CommandLineTools shim
/// at `/usr/bin/git`, then `/bin/git`. Returns `None` when neither exists, in
/// which case callers fall back to the hard-coded exec-path candidates below.
fn system_git_binary() -> Option<&'static str> {
    ["/usr/bin/git", "/bin/git"]
        .into_iter()
        .find(|&candidate| std::path::Path::new(candidate).is_file())
}

/// Ask the real system git for its exec-path (the `git-core` directory that ships
/// `git-filter-branch` and friends). Returns `None` if no system git is available
/// or it fails to report a usable directory.
fn system_git_exec_path() -> Option<std::path::PathBuf> {
    let git = system_git_binary()?;
    // The test harness exports `GIT_EXEC_PATH` pointing at a synthetic helper dir
    // (which only holds `git-p4`). With that set, `git --exec-path` simply echoes
    // the override back instead of its real `git-core` directory, so we must clear
    // it (and the GIT_DIR-style overrides) to recover git's built-in default.
    let output = Command::new(git)
        .arg("--exec-path")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?;
    let dir = dir.trim();
    if dir.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(dir);
    path.is_dir().then_some(path)
}

/// Resolve `git-filter-branch` and the exec-path directory used for helper scripts.
fn resolve_filter_branch_script() -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // The harness rewrites `$GIT_EXEC_PATH` to a synthetic helper dir that only holds
    // `git-p4`, so this rarely matches; it is still honored first for real installs.
    if let Ok(exec_path) = std::env::var("GIT_EXEC_PATH") {
        candidates.push(std::path::PathBuf::from(exec_path).join("git-filter-branch"));
    }
    // Ask the real system git where its exec-path lives. This is what makes the
    // lookup portable on macOS, where git-core lives under the Xcode toolchain or
    // Homebrew rather than the hard-coded Linux locations below.
    if let Some(exec_dir) = system_git_exec_path() {
        candidates.push(exec_dir.join("git-filter-branch"));
    }
    for dir in &[
        "/usr/lib/git-core",
        "/usr/libexec/git-core",
        "/usr/local/lib/git-core",
        "/usr/local/libexec/git-core",
    ] {
        candidates.push(std::path::Path::new(dir).join("git-filter-branch"));
    }
    for path in candidates {
        if path.is_file() {
            let exec_dir = path.parent().unwrap_or(path.as_path()).to_path_buf();
            return Ok((path, exec_dir));
        }
    }
    anyhow::bail!("cannot find git-filter-branch");
}

pub fn run(args: Args) -> Result<()> {
    let (script_path, exec_path) = resolve_filter_branch_script()?;

    // Prepend the exec path to PATH so that `git-sh-setup` and other
    // shell helpers sourced by filter-branch can be found.
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{current_path}", exec_path.display());

    // Point `GIT_EXEC_PATH` at the same real `git-core` directory so the script's
    // sourced helpers (`git-sh-i18n`, `git-sh-setup`) resolve. The test harness
    // otherwise exports a synthetic exec dir holding only `git-p4`, which would
    // make `git-sh-setup` fail to locate `git-sh-i18n` and spam stderr.
    let status = Command::new("bash")
        .arg(script_path)
        .args(&args.args)
        .env("PATH", &new_path)
        .env("GIT_EXEC_PATH", &exec_path)
        .status()
        .context("failed to run git-filter-branch")?;

    std::process::exit(status.code().unwrap_or(1));
}

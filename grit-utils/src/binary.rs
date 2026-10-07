//! Resolve executables on `PATH` without shelling out to `which`.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Resolve a binary: explicit user path, workspace `target/release/grit`, then `PATH`.
pub fn resolve_binary(name: &str, user_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = user_path {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("binary not found: {}", p.display());
    }
    if name == "grit" {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit");
        if workspace.is_file() {
            return workspace.canonicalize().context("canonicalize grit path");
        }
    }
    find_on_path(name).with_context(|| format!("could not find `{name}` on PATH"))
}

/// Locate `hyperfine` or fail with an actionable message.
pub fn require_hyperfine() -> Result<PathBuf> {
    find_on_path("hyperfine").context(
        "hyperfine is required for grit-bench but was not found on PATH. \
         Install it from https://github.com/sharkdp/hyperfine",
    )
}

/// Run `program --version` (or `-V` for grit) and return stdout trimmed.
pub fn tool_version(program: &Path) -> String {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .or_else(|| {
            std::process::Command::new(program)
                .arg("-V")
                .output()
                .ok()
                .filter(|o| o.status.success())
        });
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Best-effort commit hash for the grit tree (when this crate is built from the grit repo).
pub fn grit_source_commit() -> Option<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest.join(".."))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn find_on_path(name: &str) -> Result<PathBuf> {
    let paths = env::var_os("PATH").context("PATH is not set")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("`{name}` not found on PATH")
}

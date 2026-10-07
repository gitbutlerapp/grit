//! Git smart protocol handlers for HTTP transport.
//!
//! Provides a clean Rust API for running git upload-pack and receive-pack
//! operations. Implemented by spawning `grit upload-pack` / `grit receive-pack`
//! as a subprocess with piped I/O — the same model as `git-http-backend`.

pub mod receive_pack;
pub mod upload_pack;

use anyhow::Context;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Find the grit executable path.
///
/// Checks the `GUST_BIN` env var first (used by tests to point at a freshly built
/// binary), then the running executable when it is `grit` itself, then a `grit`
/// next to the running executable (where `grit-http-server` is usually
/// installed), and finally `grit` on `PATH`.
pub fn grit_executable() -> PathBuf {
    if let Ok(bin) = std::env::var("GUST_BIN") {
        if !bin.is_empty() {
            return PathBuf::from(bin);
        }
    }
    let program = format!("grit{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        if exe.file_name().is_some_and(|n| n == program.as_str()) {
            return exe;
        }
        if let Some(sibling) = exe.parent().map(|dir| dir.join(&program)) {
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("grit")
}

/// Run `grit <service> <args> <repo_path>` with `stdin` as its input and return
/// its stdout.
///
/// `protocol_version` is passed through `GIT_PROTOCOL`, as Git does.
///
/// # Errors
///
/// Fails when the program cannot be started or exits unsuccessfully; the error
/// carries the program's stderr.
fn run_service(
    service: &str,
    args: &[&str],
    repo_path: &Path,
    protocol_version: Option<u8>,
    stdin: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let grit = grit_executable();
    let mut cmd = Command::new(&grit);
    cmd.arg(service)
        .args(args)
        .arg(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(v) = protocol_version {
        cmd.env("GIT_PROTOCOL", format!("version={v}"));
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{} {service}'", grit.display()))?;
    // Feed stdin from a separate thread so a child that writes while it reads
    // (negotiation acknowledgements) cannot fill its stdout pipe and deadlock.
    let input = child.stdin.take();
    let body = stdin.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        if let Some(mut input) = input {
            input.write_all(&body)?;
        }
        Ok(())
    });
    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {service}"))?;
    // A child that exits before reading all of its input is reported through
    // its exit status below, so a broken pipe here is not an error of its own.
    let _ = writer.join();
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{service} exited with {}: {}", output.status, err.trim());
    }
    Ok(output.stdout)
}

/// Validate that a path looks like a git repository (bare or non-bare).
pub fn validate_repo_path(path: &Path) -> anyhow::Result<PathBuf> {
    // Bare repo: has HEAD file directly
    if path.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    // Non-bare: has .git/HEAD
    let dot_git = path.join(".git");
    if dot_git.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    anyhow::bail!("not a git repository: {}", path.display())
}

//! `grit update` — update grit to the latest release by re-running the installer.
//!
//! This re-runs the official install script, which downloads the latest pre-built
//! `grit` binary from GitHub Releases for the current platform. By default it installs
//! over the directory of the currently-running `grit` binary so the in-use install is
//! upgraded in place.
//!
//! On Unix this runs the POSIX-sh installer (`curl -fsSL <url> | sh`); on Windows it
//! runs the PowerShell installer (`irm <url> | iex`), the same scripts the documented
//! one-line installs use.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::output::{progress, HumanRender, OutputMode};

/// URL of the POSIX-sh installer (used on Unix).
#[cfg(not(windows))]
const INSTALL_URL: &str = "https://grit-scm.com/install";
/// URL of the PowerShell installer (used on Windows).
#[cfg(windows)]
const INSTALL_URL: &str = "https://grit-scm.com/install.ps1";

/// Result of `grit update`.
#[derive(Serialize)]
pub struct UpdateOutcome {
    pub updated: bool,
    /// The version of `grit` that ran the update (the new version is whatever the
    /// installer fetched).
    pub version: String,
}

impl HumanRender for UpdateOutcome {
    fn render_human(&self) {
        // The installer's own output is the user-facing feedback; nothing extra.
    }
}

/// Send the installer's stdout to our stderr in JSON mode so our stdout stays a
/// single clean object; inherit normally in human mode.
fn child_stdout(mode: OutputMode) -> Stdio {
    match mode {
        OutputMode::Json | OutputMode::Markdown => Stdio::null(),
        OutputMode::Human => Stdio::inherit(),
    }
}

/// Run the `update` command.
#[cfg(not(windows))]
pub fn run(mode: OutputMode) -> Result<UpdateOutcome> {
    // The installer is a POSIX-sh pipeline that shells out to curl + tar.
    for tool in ["sh", "curl"] {
        if find_in_path(tool).is_none() {
            bail!(
                "`{tool}` is required to update grit. Install it, or run the installer manually:\n  curl -fsSL {INSTALL_URL} | sh"
            );
        }
    }

    progress(
        mode,
        &format!("Updating grit (current: {})", env!("CARGO_PKG_VERSION")),
    );
    report_install_dir(mode);

    // Run `curl -fsSL <url> | sh`, exporting INSTALL_DIR so the installer targets our path.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("curl -fsSL {INSTALL_URL} | sh"))
        .stdout(child_stdout(mode));
    if let Some(dir) = current_exe_dir() {
        cmd.env("INSTALL_DIR", dir);
    }

    let status = cmd.status().context("running the grit installer")?;
    if !status.success() {
        bail!(
            "grit update failed: the installer exited unsuccessfully ({status}). \
             You can retry with:\n  curl -fsSL {INSTALL_URL} | sh"
        );
    }
    Ok(UpdateOutcome {
        updated: true,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

/// Run the `update` command.
#[cfg(windows)]
pub fn run(mode: OutputMode) -> Result<UpdateOutcome> {
    progress(
        mode,
        &format!("Updating grit (current: {})", env!("CARGO_PKG_VERSION")),
    );
    report_install_dir(mode);

    // Run `irm <url> | iex` under PowerShell, exporting GRIT_INSTALL_DIR so the
    // installer targets our path (the env var the .ps1 installer reads).
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &format!("irm {INSTALL_URL} | iex"),
    ])
    .stdout(child_stdout(mode));
    if let Some(dir) = current_exe_dir() {
        cmd.env("GRIT_INSTALL_DIR", dir);
    }

    let status = cmd.status().context("running the grit installer")?;
    if !status.success() {
        bail!(
            "grit update failed: the installer exited unsuccessfully ({status}). \
             You can retry with:\n  irm {INSTALL_URL} | iex"
        );
    }
    Ok(UpdateOutcome {
        updated: true,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

/// Tell the user where we're installing (the running binary's directory, or the
/// installer's own default if we can't determine it). Human mode only.
fn report_install_dir(mode: OutputMode) {
    match current_exe_dir() {
        Some(d) => progress(mode, &format!("Install directory: {}", d.display())),
        None => progress(mode, "Install directory: installer default"),
    }
}

/// The directory of the currently-running `grit` binary, so the in-use install is
/// replaced in place.
fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
}

/// Minimal `which`: first executable-or-regular file named `tool` on `$PATH`.
#[cfg(not(windows))]
fn find_in_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|cand| cand.is_file())
}

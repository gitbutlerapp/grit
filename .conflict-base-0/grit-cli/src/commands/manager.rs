//! `grit manager` — a Git credential helper backed by the Windows Credential
//! Manager.
//!
//! Git for Windows ships Git Credential Manager (the `manager` helper), but it
//! is only present when Git for Windows is installed. `grit manager` provides the
//! same capability built into `grit` itself: it speaks Git's credential-helper
//! protocol (`get` / `store` / `erase`) on stdin/stdout, storing secrets
//! directly in the Windows Credential Manager. `grit auth` wires `credential.helper`
//! to it automatically on Windows when nothing else is configured.
//!
//! You don't normally run this yourself — Git (and `grit`) invoke it. On
//! non-Windows platforms the command exists but errors, since there is no
//! Windows Credential Manager to talk to.

use std::io::Read;

use anyhow::{Context, Result};
use grit_lib::remote::credentials::Credential;

/// Run `grit manager <operation>`, where `operation` is `get`, `store`, or
/// `erase` (Git's credential-helper protocol). The credential record is read
/// from stdin; for `get`, the resolved fields are written to stdout.
pub fn run(operation: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the credential from stdin")?;
    let cred = Credential::parse(&input);

    match operation {
        "get" => get(&cred),
        "store" => store(&cred),
        "erase" => erase(&cred),
        // Git may invoke other (e.g. capability) actions; ignore them quietly,
        // as a helper that doesn't implement an action is expected to.
        _ => Ok(()),
    }
}

#[cfg(windows)]
fn get(cred: &Credential) -> Result<()> {
    use std::io::Write;

    if let Some(found) = grit_lib::remote::credentials::windows_store::get(cred)? {
        // Echo back the input identity fields so Git can match the response,
        // then the username/password we found.
        let mut response = found;
        if response.username.is_none() {
            response.username = cred.username.clone();
        }
        std::io::stdout()
            .write_all(response.serialize().as_bytes())
            .context("writing the credential to stdout")?;
    }
    Ok(())
}

#[cfg(windows)]
fn store(cred: &Credential) -> Result<()> {
    grit_lib::remote::credentials::windows_store::store(cred)?;
    Ok(())
}

#[cfg(windows)]
fn erase(cred: &Credential) -> Result<()> {
    grit_lib::remote::credentials::windows_store::erase(cred)?;
    Ok(())
}

#[cfg(not(windows))]
fn get(_cred: &Credential) -> Result<()> {
    not_windows()
}

#[cfg(not(windows))]
fn store(_cred: &Credential) -> Result<()> {
    not_windows()
}

#[cfg(not(windows))]
fn erase(_cred: &Credential) -> Result<()> {
    not_windows()
}

#[cfg(not(windows))]
fn not_windows() -> Result<()> {
    anyhow::bail!(
        "`grit manager` stores credentials in the Windows Credential Manager and is only available on Windows"
    )
}

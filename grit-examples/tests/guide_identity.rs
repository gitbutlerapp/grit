use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_identity_resolves_author_and_builds_commit_request() -> Result<()> {
    let exe = env!("CARGO_BIN_EXE_guide_identity");
    let output = Command::new(exe).output()?;
    assert!(
        output.status.success(),
        "guide_identity failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).context("stdout utf-8")?;
    assert!(stdout.contains("author=Ada Lovelace"));
    Ok(())
}

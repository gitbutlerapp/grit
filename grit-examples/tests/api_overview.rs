use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn api_overview_example_runs_on_temp_repo() -> Result<()> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .context("grit-examples has no parent directory")?;
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .args([
            "run",
            "--quiet",
            "-p",
            "grit-examples",
            "--example",
            "api_overview",
        ])
        .output()
        .context("cargo run --example api_overview")?;
    assert!(
        output.status.success(),
        "api_overview failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).context("stdout utf-8")?;
    assert!(stdout.contains("blob="), "stdout: {stdout}");
    assert!(
        stdout.contains("missing_object=object_not_found"),
        "stdout: {stdout}"
    );
    Ok(())
}

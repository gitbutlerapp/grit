use std::process::Command;

use anyhow::{Context, Result};

/// Every example moved from ``docs/examples/`` must still compile.
#[test]
fn legacy_doc_examples_build() -> Result<()> {
    let output = Command::new("cargo")
        .args(["build", "-p", "grit-examples", "--examples"])
        .output()
        .context("cargo build --examples")?;
    assert!(
        output.status.success(),
        "examples failed to build:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

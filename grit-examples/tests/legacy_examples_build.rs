use std::process::Command;

use anyhow::{Context, Result};

/// Every example moved from ``docs/examples/`` must still compile.
#[test]
fn legacy_doc_examples_build() -> Result<()> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_stderr = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let output = Command::new("cargo")
            .args(["build", "-p", "grit-examples", "--examples"])
            .env("CARGO_BUILD_JOBS", "1")
            .output()
            .context("cargo build --examples")?;
        if output.status.success() {
            return Ok(());
        }
        last_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let retryable = last_stderr.contains("Bus error")
            || last_stderr.contains("signal 7")
            || last_stderr.contains("ld terminated");
        if !retryable || attempt + 1 == MAX_ATTEMPTS {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    panic!("examples failed to build after {MAX_ATTEMPTS} attempts:\n{last_stderr}");
}

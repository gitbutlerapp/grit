use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_bundle_verify_with_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;
    Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("f"), b"x\n")?;
    Command::new("git")
        .args(["add", "f"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "only"])
        .current_dir(repo)
        .output()?;

    let bundle = repo.join("guide.bundle");
    let exe = env!("CARGO_BIN_EXE_guide_bundle");
    let output = Command::new(exe)
        .args([repo.as_os_str(), bundle.as_os_str()])
        .output()?;
    assert!(
        output.status.success(),
        "guide_bundle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("bundle_ok=true"));

    let verify = Command::new("git")
        .args(["bundle", "verify", bundle.to_str().unwrap()])
        .current_dir(repo)
        .output()?;
    assert!(
        verify.status.success(),
        "git bundle verify: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    Ok(())
}

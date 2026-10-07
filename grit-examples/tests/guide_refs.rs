use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_refs_matches_system_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;
    Command::new("git")
        .args(["config", "user.email", "ada@example.com"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["config", "user.name", "Ada"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "seed"])
        .current_dir(repo)
        .output()
        .context("git commit")?;

    let exe = env!("CARGO_BIN_EXE_guide_refs");
    let output = Command::new(exe).arg(repo).output()?;
    assert!(
        output.status.success(),
        "guide_refs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;

    let demo_oid = stdout
        .lines()
        .find_map(|l| l.strip_prefix("demo_oid="))
        .context("demo_oid line")?;

    let git_rev = Command::new("git")
        .args(["rev-parse", "refs/heads/library-guide-demo"])
        .current_dir(repo)
        .output()?;
    assert!(git_rev.status.success());
    let git_oid = String::from_utf8(git_rev.stdout)?.trim().to_owned();
    assert_eq!(git_oid, demo_oid);

    let git_reflog = Command::new("git")
        .args([
            "reflog",
            "show",
            "--format=%H %gs",
            "refs/heads/library-guide-demo",
        ])
        .current_dir(repo)
        .output()?;
    assert!(git_reflog.status.success());
    let reflog_text = String::from_utf8(git_reflog.stdout)?;
    assert!(reflog_text.contains(demo_oid));
    assert!(reflog_text.contains("library guide refs example"));

    Ok(())
}

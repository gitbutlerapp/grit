//! Reset must not require strict commit identity (reflog-only; issue #1034 review).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run grit")
}

#[test]
fn reset_hard_succeeds_without_configured_identity() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();
    assert!(run_grit(root, &["init"], &[]).status.success());
    assert!(run_grit(root, &["config", "user.name", "Seed"], &[])
        .status
        .success());
    assert!(
        run_grit(root, &["config", "user.email", "seed@example.com"], &[])
            .status
            .success()
    );
    fs::write(root.join("a.txt"), b"v1\n")?;
    assert!(run_grit(root, &["commit", "-m", "seed"], &[])
        .status
        .success());
    fs::write(root.join("a.txt"), b"v2\n")?;
    assert!(run_grit(root, &["commit", "-m", "second"], &[])
        .status
        .success());

    fs::write(root.join("a.txt"), b"dirty\n")?;
    assert!(run_grit(root, &["config", "--unset", "user.name"], &[])
        .status
        .success());
    assert!(run_grit(root, &["config", "--unset", "user.email"], &[])
        .status
        .success());

    let reset = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["reset", "--hard", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("EMAIL")
        .output()?;
    assert!(
        reset.status.success(),
        "reset --hard should not require strict identity: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let content = fs::read_to_string(root.join("a.txt"))?;
    assert_eq!(content, "v2\n");
    Ok(())
}

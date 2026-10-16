//! Amend keeps the prior author; committer may come from GIT_COMMITTER_* alone (#1034 review).

use std::error::Error;
use std::fs;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

#[test]
fn amend_with_committer_env_only_preserves_author() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();

    let seed_env = [
        ("GIT_AUTHOR_NAME", "Original Author"),
        ("GIT_AUTHOR_EMAIL", "author@example.com"),
        ("GIT_COMMITTER_NAME", "Original Author"),
        ("GIT_COMMITTER_EMAIL", "author@example.com"),
    ];
    let mut init = Command::new(env!("CARGO_BIN_EXE_grit"));
    init.current_dir(root)
        .args(["init"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config());
    for (k, v) in seed_env {
        init.env(k, v);
    }
    assert!(init.output()?.status.success());

    fs::write(root.join("f.txt"), b"x\n")?;
    let mut first = Command::new(env!("CARGO_BIN_EXE_grit"));
    first
        .current_dir(root)
        .args(["commit", "-m", "first"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config());
    for (k, v) in seed_env {
        first.env(k, v);
    }
    assert!(first.output()?.status.success());

    fs::write(root.join("f.txt"), b"y\n")?;
    let amend = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["commit", "--amend", "-m", "amended"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env("GIT_COMMITTER_NAME", "New Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("EMAIL")
        .output()?;
    assert!(
        amend.status.success(),
        "amend with committer-only env failed: {}",
        String::from_utf8_lossy(&amend.stderr)
    );

    let show = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["show", "--json"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()?;
    assert!(show.status.success());
    let v: serde_json::Value = serde_json::from_slice(&show.stdout)?;
    assert_eq!(
        v["commit"]["author"]["name"].as_str(),
        Some("Original Author")
    );
    assert_eq!(
        v["commit"]["author"]["email"].as_str(),
        Some("author@example.com")
    );
    assert_eq!(
        v["commit"]["committer"]["name"].as_str(),
        Some("New Committer")
    );
    assert_eq!(
        v["commit"]["committer"]["email"].as_str(),
        Some("committer@example.com")
    );
    Ok(())
}

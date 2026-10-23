//! `grit clone --bare` and `--no-checkout` JSON output.

use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn clone_bare_json_reports_bare_and_no_files() -> TestResult {
    let base = tempfile::tempdir()?;
    let upstream = base.path().join("upstream");
    std::fs::create_dir_all(&upstream)?;
    git_in(&upstream, &["init", "-q", "-b", "main"]);
    git_in(
        &upstream,
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "--allow-empty",
            "-qm",
            "c0",
        ],
    );

    let dest = base.path().join("repo.git");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--json",
            "--bare",
            upstream.to_str().unwrap(),
            dest.to_str().unwrap(),
        ])
        .output()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(parsed["bare"], true);
    assert_eq!(parsed["files"], 0);
    assert_eq!(parsed["no_checkout_reason"], "bare");
    Ok(())
}

#[test]
fn clone_no_checkout_json() -> TestResult {
    let base = tempfile::tempdir()?;
    let upstream = base.path().join("upstream");
    std::fs::create_dir_all(&upstream)?;
    git_in(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("f"), b"x\n")?;
    git_in(&upstream, &["add", "f"]);
    git_in(
        &upstream,
        &["-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-qm", "c1"],
    );

    let dest = base.path().join("dest");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--json",
            "--no-checkout",
            upstream.to_str().unwrap(),
            dest.to_str().unwrap(),
        ])
        .output()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(parsed["bare"], false);
    assert_eq!(parsed["files"], 0);
    assert_eq!(parsed["no_checkout_reason"], "no_checkout");
    Ok(())
}

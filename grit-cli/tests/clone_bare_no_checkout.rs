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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "-qm",
            "c1",
        ],
    );

    std::fs::write(upstream.join("f"), b"second\n")?;
    git_in(
        &upstream,
        &[
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=a",
            "commit",
            "-qam",
            "c2",
        ],
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(parsed["bare"], false);
    assert_eq!(parsed["files"], 0);
    assert_eq!(parsed["no_checkout_reason"], "no_checkout");
    assert!(!dest.join(".git/index").exists());
    assert!(!dest.join("f").exists());
    let head_before = std::fs::read(dest.join(".git/HEAD"))?;
    let out = Command::new(GRIT)
        .args([
            "-C",
            dest.to_str().unwrap(),
            "switch",
            "--detach",
            "HEAD~1",
            "--dry-run",
            "--json",
        ])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let preview: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["changed_paths"], 1);
    assert_eq!(std::fs::read(dest.join(".git/HEAD"))?, head_before);
    assert!(!dest.join(".git/index").exists());
    assert!(!dest.join("f").exists());

    let out = Command::new(GRIT)
        .args([
            "-C",
            dest.to_str().unwrap(),
            "switch",
            "--detach",
            "HEAD~1",
            "--json",
        ])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let switched: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(switched["detached"], true);
    assert!(switched["branch"].is_null());
    assert_eq!(switched["commit"], preview["commit"]);
    assert_eq!(std::fs::read(dest.join("f"))?, b"x\n");
    assert!(dest.join(".git/index").is_file());
    let status = Command::new("git")
        .current_dir(&dest)
        .args(["status", "--porcelain"])
        .output()?;
    assert!(status.status.success());
    assert!(status.stdout.is_empty());
    git_in(&dest, &["fsck", "--strict"]);
    Ok(())
}

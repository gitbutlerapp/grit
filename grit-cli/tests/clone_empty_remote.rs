//! `grit clone` succeeds for an empty bare remote (issue #991).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        dir.display()
    );
    Ok(())
}

#[test]
fn clone_empty_bare_exits_zero_and_creates_directory() -> TestResult {
    let base = tempfile::tempdir()?;
    let origin = base.path().join("origin.git");
    fs::create_dir_all(&origin)?;
    git_in(
        base.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    )?;

    let dest = base.path().join("A");
    let out = Command::new(GRIT)
        .args(["clone", origin.to_str().unwrap(), dest.to_str().unwrap()])
        .output()?;
    assert!(
        out.status.success(),
        "grit clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.is_dir());
    assert!(dest.join(".git").is_dir());

    let json = Command::new(GRIT)
        .args([
            "clone",
            "--json",
            origin.to_str().unwrap(),
            base.path().join("B").to_str().unwrap(),
        ])
        .output()?;
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(parsed["files"], 0);
    assert_eq!(parsed["branch"], "main");
    assert_eq!(parsed["no_checkout_reason"], "empty_remote");

    Ok(())
}

#[test]
#[cfg(unix)]
fn clone_empty_bare_over_ssh_fake_transport_exits_zero() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let base = tempfile::tempdir()?;
    let origin = base.path().join("origin.git");
    fs::create_dir_all(&origin)?;
    git_in(
        base.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    )?;

    let fake_ssh = base.path().join("fake-ssh.sh");
    fs::write(
        &fake_ssh,
        br#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
esac
eval "exec $cmd"
"#,
    )?;
    let mut perms = fs::metadata(&fake_ssh)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_ssh, perms)?;

    let abs = origin.to_str().unwrap();
    let url = format!("ssh://localhost{abs}");
    let dest = base.path().join("ssh-clone");
    let out = Command::new(GRIT)
        .args(["clone", &url, dest.to_str().unwrap()])
        .env("GIT_SSH", &fake_ssh)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(
        out.status.success(),
        "grit clone over ssh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.join(".git").is_dir());

    Ok(())
}

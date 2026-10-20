//! `grit auth logout` against real credential helpers, cross-checked with the
//! system `git credential` command.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn isolated(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
}

fn git(home: &Path, args: &[&str], stdin: Option<&[u8]>) -> Option<Output> {
    let mut cmd = Command::new("git");
    isolated(&mut cmd, home);
    let mut child = cmd
        .current_dir(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(input) = stdin {
        child.stdin.as_mut()?.write_all(input).ok()?;
    }
    child.wait_with_output().ok()
}

fn grit_logout_json(home: &Path) -> Value {
    let mut cmd = Command::new(GRIT);
    isolated(&mut cmd, home);
    let out = cmd
        .current_dir(home)
        .args(["--json", "auth", "logout"])
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit auth logout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("logout JSON")
}

fn home_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().to_path_buf();
    (tmp, home)
}

const TOKEN_QUERY: &[u8] = b"protocol=https\nhost=github.com\nusername=x-access-token\n\n";

#[test]
fn logout_without_helper_reports_nothing_removed() {
    let (_tmp, home) = home_dir();
    if git(&home, &["--version"], None).is_none() {
        eprintln!("skipping: system git unavailable");
        return;
    }
    let json = grit_logout_json(&home);
    assert_eq!(json["logged_out"], Value::Bool(false));
    assert_eq!(json["host"], "github.com");
}

#[test]
fn logout_erases_token_that_git_credential_stored() {
    let (_tmp, home) = home_dir();
    let store = home.join("token-store");
    let helper = format!("store --file={}", store.display());
    let Some(out) = git(
        &home,
        &["config", "--global", "credential.helper", &helper],
        None,
    ) else {
        eprintln!("skipping: system git unavailable");
        return;
    };
    assert!(out.status.success(), "git config: {out:?}");

    let approve = git(
        &home,
        &["credential", "approve"],
        Some(b"protocol=https\nhost=github.com\nusername=x-access-token\npassword=gho_cli\n\n"),
    )
    .expect("git credential approve");
    assert!(
        approve.status.success(),
        "git credential approve: {approve:?}"
    );
    let filled = git(&home, &["credential", "fill"], Some(TOKEN_QUERY)).expect("fill");
    assert!(String::from_utf8_lossy(&filled.stdout).contains("password=gho_cli"));

    let json = grit_logout_json(&home);
    assert_eq!(json["logged_out"], Value::Bool(true));

    let after = std::fs::read_to_string(&store).unwrap_or_default();
    assert!(!after.contains("gho_cli"), "token still stored: {after}");
    let filled = git(&home, &["credential", "fill"], Some(TOKEN_QUERY)).expect("fill");
    assert!(!String::from_utf8_lossy(&filled.stdout).contains("password="));
}

#[test]
fn logout_ignores_helpers_scoped_to_other_hosts() {
    let (_tmp, home) = home_dir();
    let Some(out) = git(
        &home,
        &[
            "config",
            "--global",
            "credential.https://example.com.helper",
            "store",
        ],
        None,
    ) else {
        eprintln!("skipping: system git unavailable");
        return;
    };
    assert!(out.status.success(), "git config: {out:?}");
    let json = grit_logout_json(&home);
    assert_eq!(json["logged_out"], Value::Bool(false));
}

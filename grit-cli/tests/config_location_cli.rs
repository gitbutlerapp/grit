//! CLI tests for `grit config --local`, `-f`, and get origin JSON.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-config-cli-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit")
}

fn grit_ok(dir: &Path, args: &[&str]) {
    let out = grit(dir, args);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_get_missing_key_json_null_and_exit_code() -> TestResult {
    let scratch = Scratch::new("config-missing")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let out = grit(&repo, &["--json", "config", "no.such.key"]);
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["action"], "get");
    assert_eq!(v["key"], "no.such.key");
    assert!(v["value"].is_null());
    assert!(v["scope"].is_null());
    Ok(())
}

#[test]
fn config_file_flag_reads_and_writes_outside_repo() -> TestResult {
    let scratch = Scratch::new("config-file-flag")?;
    let cfg = scratch.path().join("alone.cfg");

    let set = grit(
        scratch.path(),
        &["config", "-f", cfg.to_str().unwrap(), "demo.value", "42"],
    );
    assert!(set.status.success());

    let got = grit(
        scratch.path(),
        &[
            "config",
            "-f",
            cfg.to_str().unwrap(),
            "--json",
            "demo.value",
        ],
    );
    assert!(got.status.success());
    let v: serde_json::Value = serde_json::from_slice(&got.stdout)?;
    assert_eq!(v["value"], "42");
    assert_eq!(v["scope"], "command");
    assert!(v["file"].as_str().unwrap().ends_with("alone.cfg"));
    assert!(v["line"].as_u64().unwrap() >= 1);
    Ok(())
}

#[test]
fn config_get_git_config_parameters_has_command_scope_no_file() -> TestResult {
    let scratch = Scratch::new("config-cmd-env")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_PARAMETERS", "'demo.value=42'")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--json", "demo.value"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["value"], "42");
    assert_eq!(v["scope"], "command");
    assert!(v["file"].is_null());
    assert!(v["line"].is_null());
    Ok(())
}

#[test]
fn config_local_flag() -> TestResult {
    let scratch = Scratch::new("config-local-flag")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    grit_ok(&repo, &["init", "."]);

    let global_cfg = scratch.path().join("global.cfg");
    fs::write(&global_cfg, "[user]\n\tname = Global Only\n")?;

    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--local", "--json", "user.name"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert!(v["value"].is_null());

    grit_ok(&repo, &["config", "user.name", "Local Name"]);
    let mut cmd = Command::new(GRIT);
    cmd.env("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["config", "--local", "--json", "user.name"])
        .current_dir(&repo);
    let out = cmd.output()?;
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v["value"], "Local Name");
    assert_eq!(v["scope"], "local");
    Ok(())
}

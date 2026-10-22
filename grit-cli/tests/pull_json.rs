//! `grit pull --json` result values and exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use grit_test_support::{git, unique_tmp, write_file};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct CmdOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn grit_cmd(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    CmdOutput {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn gs_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = grit_cmd(dir, &full);
    assert!(out.ok, "grit failed: {}", out.stderr);
    serde_json::from_str(&out.stdout).expect("json")
}

fn commit(dir: &Path, file: &str, msg: &str) {
    write_file(dir, file, msg);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
}

#[test]
fn pull_json_fast_forward_exit_zero() -> TestResult {
    let root = unique_tmp("pull-json", "ff");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&upstream, "a", "remote");
    git(&client, &["fetch", "-q"]);
    let json = gs_json(&client, &["pull"]);
    assert_eq!(json["result"], "fast_forward");
    Ok(())
}

#[test]
fn pull_json_refused_not_ff_exit_one() -> TestResult {
    let root = unique_tmp("pull-json", "refused");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "a1");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&client, "b", "local");
    commit(&upstream, "a", "remote");
    git(&client, &["fetch", "-q"]);
    let out = grit_cmd(&client, &["--json", "pull", "--ff-only"]);
    assert!(!out.ok, "ff-only refused must exit 1");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "refused_not_ff");
    assert!(json["ahead"].as_u64().is_some());
    assert!(json["behind"].as_u64().is_some());
    Ok(())
}

#[test]
fn pull_json_conflicts_exit_one() -> TestResult {
    let root = unique_tmp("pull-json", "conflicts");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    write_file(&upstream, "f", "base\n");
    git(&upstream, &["add", "f"]);
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    write_file(&client, "f", "local\n");
    git(&client, &["add", "f"]);
    git(
        &client,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "local",
        ],
    );
    write_file(&upstream, "f", "remote\n");
    git(&upstream, &["add", "f"]);
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "remote",
        ],
    );
    git(&client, &["fetch", "-q"]);
    let out = grit_cmd(&client, &["--json", "pull"]);
    assert!(!out.ok, "conflicts must exit 1");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert!(json["conflicts"].as_array().is_some());
    Ok(())
}

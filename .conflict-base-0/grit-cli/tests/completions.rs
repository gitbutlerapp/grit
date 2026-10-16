//! Integration tests for `grit completions`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

#[test]
fn bash_completions_exit_zero_and_cover_cli() {
    let output = Command::new(GRIT)
        .args(["completions", "bash"])
        .output()
        .expect("spawn grit");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(script.contains("status"));
    assert!(script.contains("commit"));
    assert!(script.contains("fetch"));
    assert!(script.contains("--json"));
}

#[test]
fn zsh_completions_exit_zero_and_cover_cli() {
    let output = Command::new(GRIT)
        .args(["completions", "zsh"])
        .output()
        .expect("spawn grit");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(script.contains("status"));
    assert!(script.contains("commit"));
    assert!(script.contains("fetch"));
    assert!(script.contains("--json"));
}

#[test]
fn fish_completions_exit_zero_and_cover_cli() {
    let output = Command::new(GRIT)
        .args(["completions", "fish"])
        .output()
        .expect("spawn grit");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(script.contains("status"));
    assert!(script.contains("commit"));
    assert!(script.contains("fetch"));
    assert!(script.contains("--json"));
}

//! `grit-bench daily --list` exposes executable scenario ids.

use std::process::Command;

const WORKSPACE_EXTRA_IDS: [&str; 8] = [
    "stash/push-paths@synth-10k",
    "stash/pop-index@synth-10k",
    "reset/keep@synth-10k",
    "rm/dry-run@synth-10k",
    "config/get-origin@small",
    "worktree/list@synth-10k",
    "clean/only-ignored@synth-10k",
    "init/branch@small",
];

#[test]
fn daily_list_includes_workspace_extras() {
    let bench = std::path::PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let out = Command::new(&bench)
        .args(["daily", "--list"])
        .output()
        .expect("spawn grit-bench daily --list");
    assert!(
        out.status.success(),
        "daily --list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    for id in WORKSPACE_EXTRA_IDS {
        assert!(
            listing.lines().any(|line| line.trim() == id),
            "missing {id} in daily --list output:\n{listing}"
        );
    }
}

#[test]
fn daily_list_includes_bounded_log_tag_layouts() {
    let out = Command::new(env!("CARGO_BIN_EXE_grit-bench"))
        .args(["daily", "--list"])
        .output()
        .expect("daily list");
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    for id in [
        "log/json-n10-loose-tags@git-git",
        "log/json-n10-packed-tags@git-git",
    ] {
        assert!(listing.lines().any(|line| line == id), "missing {id}");
    }
}

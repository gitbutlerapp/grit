//! Git-compatible C-style path quoting (`core.quotepath`, t3300-style funny names).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use grit_lib::quote_path::quote_c_style;

fn init_git_repo(wt: &std::path::Path) {
    assert!(Command::new("git")
        .args(["init", wt.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
}

#[test]
fn quote_c_style_matches_git_status_short_for_control_bytes() {
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    init_git_repo(wt);

    let names = [
        "evil\x1b[31mRED\x1b[0m.txt",
        "bell\x07.txt",
        "ctl\x01char.txt",
    ];
    for name in names {
        std::fs::File::create(wt.join(name)).expect("create");
    }

    let git_out = Command::new("git")
        .args(["status", "--short"])
        .current_dir(wt)
        .output()
        .expect("git status");
    assert!(git_out.status.success());
    let git_lines = String::from_utf8_lossy(&git_out.stdout);

    for name in names {
        let quoted = quote_c_style(name, true);
        assert!(
            git_lines.contains(quoted.as_str()),
            "git status --short should contain grit quote for {name:?}:\n{git_lines}\nquoted={quoted}"
        );
    }
}

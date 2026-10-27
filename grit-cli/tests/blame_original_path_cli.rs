//! JSON source locations remain navigable through rename-and-edit history.
#![allow(clippy::unwrap_used)]

use grit_test_support::git;
use serde_json::Value;
use std::process::Command;

#[test]
fn rename_edit_then_subtree_move_preserves_json_line_and_hunk_origins() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    let body: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    std::fs::write(root.join("original"), &body).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    let origin = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    git(root, &["mv", "original", "renamed"]);
    std::fs::write(
        root.join("renamed"),
        body.replace("line 6\n", "edited middle\ninserted middle\n"),
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "rename and edit"]);
    std::fs::create_dir(root.join("vendor")).unwrap();
    git(root, &["mv", "renamed", "vendor/final"]);
    git(root, &["commit", "-qm", "subtree move"]);

    for range in ["1,3", "11,13"] {
        for grouped in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
            cmd.current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .args(["blame", "--json", "-L", range]);
            if grouped {
                cmd.arg("--group");
            }
            let output = cmd.arg("vendor/final").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let records = value[if grouped { "hunks" } else { "lines" }]
                .as_array()
                .unwrap();
            assert!(!records.is_empty());
            for row in records {
                assert_eq!(row["commit"], origin);
                assert_eq!(row["original_path"], "original");
                let source = git(
                    root,
                    &[
                        "show",
                        &format!(
                            "{}:{}",
                            row["commit"].as_str().unwrap(),
                            row["original_path"].as_str().unwrap()
                        ),
                    ],
                );
                let first = row[if grouped {
                    "original_start"
                } else {
                    "original_line"
                }]
                .as_u64()
                .unwrap() as usize;
                if grouped {
                    let lines = row["lines"].as_array().unwrap();
                    assert_eq!(row["count"].as_u64().unwrap() as usize, lines.len());
                    for (offset, line) in lines.iter().enumerate() {
                        assert_eq!(source.lines().nth(first + offset - 1), line.as_str());
                    }
                } else {
                    assert_eq!(source.lines().nth(first - 1), row["content"].as_str());
                }
            }
        }
    }
}

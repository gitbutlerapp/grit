//! `grit diff -U` matches Git unified hunk context.

use std::process::Command;

use grit_test_support::git;

fn grit_diff(root: &std::path::Path, extra: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
    cmd.current_dir(root).arg("diff");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("grit diff");
    assert!(
        out.status.success(),
        "grit diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_diff_unified(root: &std::path::Path, u: usize) -> String {
    git(
        root,
        &["diff", &format!("-U{u}"), "--no-color", "--no-ext-diff"],
    )
}

fn hunk_range_header(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("@@ ") {
        if let Some(idx) = rest.find(" @@") {
            return format!("@@ {}{}", &rest[..idx], " @@");
        }
    }
    line.to_string()
}

fn hunk_header_lines(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter(|l| l.starts_with("@@ "))
        .map(hunk_range_header)
        .collect()
}

#[test]
fn diff_unified_flag_matches_git_hunk_ranges() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(
        root.join("file.txt"),
        (0..30).map(|i| format!("line {i}\n")).collect::<String>(),
    )
    .expect("write");
    git(root, &["add", "file.txt"]);
    git(root, &["commit", "-qm", "base"]);

    let mut content = String::new();
    for i in 0..30 {
        if i == 15 {
            content.push_str("CHANGED\n");
        } else {
            content.push_str(&format!("line {i}\n"));
        }
    }
    std::fs::write(root.join("file.txt"), content).expect("rewrite");

    for u in [0usize, 1, 10] {
        let grit_out = grit_diff(root, &[&format!("-U{u}")]);
        let git_out = git_diff_unified(root, u);
        let grit_hunks = hunk_header_lines(&grit_out);
        let git_hunks = hunk_header_lines(&git_out);
        assert_eq!(
            grit_hunks, git_hunks,
            "hunk headers for -U{u} should match git (grit: {grit_hunks:?}, git: {git_hunks:?})"
        );
    }
}

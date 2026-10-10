//! Structured diff metadata matches system `git diff` hunk line counts.

use grit_lib::diff::structured::{format_hunk_range, structured_text_diff, StructuredDiffOptions};
use std::process::Command;

fn git_diff(repo: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["diff"])
        .current_dir(repo)
        .output()
        .expect("git diff");
    assert!(out.status.success(), "git diff failed");
    String::from_utf8(out.stdout).expect("utf8")
}

fn parse_first_hunk_header(patch: &str) -> (usize, usize, usize, usize) {
    let line = patch
        .lines()
        .find(|l| l.starts_with("@@"))
        .expect("hunk header");
    // @@ -1,2 +1,2 @@
    let rest = line.strip_prefix("@@ ").expect("prefix");
    let (old_part, new_part) = rest.split_once(" +").expect("plus");
    let old_part = old_part.strip_prefix('-').expect("minus old");
    let (old_start, old_lines) = parse_range(old_part);
    let new_part = new_part.split(" @@").next().expect("closing");
    let (new_start, new_lines) = parse_range(new_part);
    (old_start, old_lines, new_start, new_lines)
}

fn parse_range(s: &str) -> (usize, usize) {
    if let Some((a, b)) = s.split_once(',') {
        (a.parse().expect("start"), b.parse().expect("count"))
    } else {
        (s.parse().expect("start"), 1)
    }
}

#[test]
fn structured_hunk_counts_match_git_diff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("f.txt"), b"x\nmore\n").expect("write");
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(root)
        .status()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "t@e.com"])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "T"])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(root)
        .status()
        .unwrap();

    std::fs::write(root.join("f.txt"), b"x\n").expect("rewrite");
    let patch = git_diff(root);
    let (g_os, g_ol, g_ns, g_nl) = parse_first_hunk_header(&patch);

    let d = structured_text_diff(
        "x\nmore\n",
        "x\n",
        StructuredDiffOptions { context_lines: 3 },
    );
    let h = &d.hunks[0];
    assert_eq!(h.old_start, g_os);
    assert_eq!(h.old_lines, g_ol);
    assert_eq!(h.new_start, g_ns);
    assert_eq!(h.new_lines, g_nl);
    assert_eq!(
        format_hunk_range(h.old_start, h.old_lines),
        if g_ol == 1 {
            g_os.to_string()
        } else {
            format!("{g_os},{g_ol}")
        }
    );
}

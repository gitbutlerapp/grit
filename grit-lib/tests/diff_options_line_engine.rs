//! Structured hunk boundaries vs system `git diff` for each line algorithm.

mod support;

use grit_lib::diff::structured::structured_text_diff;
use grit_lib::diff::{DiffAlgorithm, DiffOptions};
use support::{blob_pairs, LineDiffAlgo};

fn blob_text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf8 fixture")
}

fn parse_hunk_header(patch: &str) -> (usize, usize, usize, usize) {
    let line = patch.lines().find(|l| l.starts_with("@@")).expect("hunk");
    let rest = line.strip_prefix("@@ ").expect("prefix");
    let (old_part, new_part) = rest.split_once(" +").expect("plus");
    let old_part = old_part.strip_prefix('-').expect("minus");
    let (os, ol) = parse_range(old_part.split(' ').next().expect("old"));
    let new_part = new_part.split(" @@").next().expect("close");
    let (ns, nl) = parse_range(new_part);
    (os, ol, ns, nl)
}

fn parse_range(s: &str) -> (usize, usize) {
    if let Some((a, b)) = s.split_once(',') {
        (a.parse().expect("start"), b.parse().expect("count"))
    } else {
        (s.parse().expect("start"), 1)
    }
}

fn git_diff_blob(old: &str, new: &str, algo: LineDiffAlgo, indent: bool) -> String {
    use std::process::Command;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("f.txt");
    std::fs::write(&path, old).expect("write old");
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(tmp.path())
        .status()
        .expect("git init");
    for (k, v) in [("user.email", "t@e.com"), ("user.name", "T")] {
        Command::new("git")
            .args(["config", k, v])
            .current_dir(tmp.path())
            .status()
            .unwrap();
    }
    Command::new("git")
        .args(["add", "f.txt"])
        .current_dir(tmp.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(tmp.path())
        .status()
        .unwrap();
    std::fs::write(&path, new).expect("write new");
    let mut args = vec!["diff", "-U3"];
    match algo {
        LineDiffAlgo::Myers => {}
        LineDiffAlgo::Histogram => args.push("--histogram"),
        LineDiffAlgo::Patience => args.push("--patience"),
        LineDiffAlgo::Minimal => args.push("--minimal"),
    }
    if indent {
        args.push("--indent-heuristic");
    } else {
        args.push("--no-indent-heuristic");
    }
    args.push("f.txt");
    let out = Command::new("git")
        .args(&args)
        .current_dir(tmp.path())
        .output()
        .expect("git diff");
    String::from_utf8(out.stdout).expect("utf8")
}

fn algo_from_line(line: LineDiffAlgo) -> DiffAlgorithm {
    match line {
        LineDiffAlgo::Myers => DiffAlgorithm::Myers,
        LineDiffAlgo::Histogram => DiffAlgorithm::Histogram,
        LineDiffAlgo::Patience => DiffAlgorithm::Patience,
        LineDiffAlgo::Minimal => DiffAlgorithm::Minimal,
    }
}

fn assert_first_hunk_matches_git(old: &str, new: &str, algo: LineDiffAlgo, indent_on: bool) {
    let patch = git_diff_blob(old, new, algo, indent_on);
    assert!(patch.contains("@@"), "expected git hunks");
    let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
    let d = structured_text_diff(
        old,
        new,
        DiffOptions::default()
            .with_context_lines(3)
            .with_word_level_inline(false)
            .with_algorithm(algo_from_line(algo))
            .with_indent_heuristic(indent_on),
    );
    let h = &d.hunks[0];
    assert_eq!(
        (h.old_start, h.old_lines, h.new_start, h.new_lines),
        (g_os, g_ol, g_ns, g_nl),
        "algo={algo:?} indent={indent_on}"
    );
}

#[test]
fn structured_hunks_match_git_for_each_algorithm() {
    let pairs = blob_pairs();
    let patience = pairs
        .iter()
        .find(|p| p.name == "t4033/t4050-mixed-layout")
        .expect("patience pair");
    let indent = pairs
        .iter()
        .find(|p| p.name == "t4061-spaces")
        .expect("indent pair");
    let old_p = blob_text(patience.old);
    let new_p = blob_text(patience.new);
    let old_i = blob_text(indent.old);
    let new_i = blob_text(indent.new);

    for algo in [
        LineDiffAlgo::Myers,
        LineDiffAlgo::Minimal,
        LineDiffAlgo::Patience,
        LineDiffAlgo::Histogram,
    ] {
        for indent_on in [false, true] {
            assert_first_hunk_matches_git(old_p, new_p, algo, indent_on);
        }
    }

    for indent_on in [false, true] {
        assert_first_hunk_matches_git(old_i, new_i, LineDiffAlgo::Myers, indent_on);
    }
}

#[test]
fn word_level_inline_does_not_change_hunk_boundaries() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4033/t4050-unique")
        .expect("pair");
    let line_opts = DiffOptions::default().with_word_level_inline(false);
    let word_opts = DiffOptions::default().with_word_level_inline(true);
    let old = blob_text(pair.old);
    let new = blob_text(pair.new);
    let line = structured_text_diff(old, new, line_opts);
    let word = structured_text_diff(old, new, word_opts);
    assert_eq!(line.hunks.len(), word.hunks.len());
    for (a, b) in line.hunks.iter().zip(word.hunks.iter()) {
        assert_eq!(
            (a.old_start, a.old_lines, a.new_start, a.new_lines),
            (b.old_start, b.old_lines, b.new_start, b.new_lines)
        );
    }
    assert!(
        word.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .any(|l| l.segments.iter().any(|s| s.emphasis)),
        "word-level path should emphasize at least one segment"
    );
}

//! Structured hunk boundaries vs system `git diff` for each line algorithm.

mod support;

use grit_lib::diff::structured::structured_text_diff;
use grit_lib::diff::{DiffAlgorithm, DiffOptions};
use support::{blob_pairs, git_diff_no_index_patch, LineDiffAlgo, WhitespaceMode};

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
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(tmp.path())
            .status()
            .expect("git init")
            .success(),
        "git init failed"
    );
    for (k, v) in [("user.email", "t@e.com"), ("user.name", "T")] {
        assert!(
            Command::new("git")
                .args(["config", k, v])
                .current_dir(tmp.path())
                .status()
                .expect("git config")
                .success(),
            "git config {k} failed"
        );
    }
    assert!(
        Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(tmp.path())
            .status()
            .expect("git add")
            .success(),
        "git add failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(tmp.path())
            .status()
            .expect("git commit")
            .success(),
        "git commit failed"
    );
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

fn seeded_u0_text_pair(seed: u32) -> (String, String) {
    let mut state = u64::from(seed).wrapping_add(0x9E37_79B9);
    let mut rnd = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    let line_count = 8 + (rnd() % 40) as usize;
    let mut old = String::new();
    let mut new = String::new();
    for line in 0..line_count {
        let indent = (rnd() % 12) as usize;
        let sp = " ".repeat(indent);
        let ch = (b'a' + (rnd() % 26) as u8) as char;
        let line_body = format!("{sp}{ch}{line}");
        old.push_str(&line_body);
        old.push('\n');
        match rnd() % 7 {
            0 => {}
            1 => {
                new.push_str(&format!("{sp}{ch}X{line}"));
                new.push('\n');
            }
            2 => {
                new.push_str(&format!("{sp}Y{line}"));
                new.push('\n');
            }
            _ => {
                new.push_str(&line_body);
                new.push('\n');
            }
        }
    }
    (old, new)
}

fn grit_u0_hunk_headers(
    old: &str,
    new: &str,
    algo: DiffAlgorithm,
    indent: bool,
) -> Vec<(usize, usize, usize, usize)> {
    let d = structured_text_diff(
        old,
        new,
        DiffOptions::default()
            .with_context_lines(0)
            .with_inter_hunk_context(0)
            .with_algorithm(algo)
            .with_indent_heuristic(indent),
    );
    d.hunks
        .iter()
        .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
        .collect()
}

fn git_u0_hunk_headers(
    old: &str,
    new: &str,
    algo: LineDiffAlgo,
    indent: bool,
) -> Vec<(usize, usize, usize, usize)> {
    let indent_arg = if indent {
        "--indent-heuristic"
    } else {
        "--no-indent-heuristic"
    };
    let patch = git_diff_no_index_patch(
        old.as_bytes(),
        new.as_bytes(),
        "f.txt",
        algo,
        WhitespaceMode::None,
        &["-U0", "--inter-hunk-context=0", indent_arg],
    );
    patch
        .lines()
        .filter(|l| l.starts_with("@@ "))
        .map(|line| {
            let rest = line.strip_prefix("@@ ").expect("prefix");
            let (old_part, new_part) = rest.split_once(" +").expect("plus");
            let old_part = old_part.strip_prefix('-').expect("minus");
            let (os, ol) = parse_range(old_part.split(' ').next().expect("old"));
            let new_part = new_part.split(" @@").next().expect("close");
            let (ns, nl) = parse_range(new_part);
            (os, ol, ns, nl)
        })
        .collect()
}

#[test]
fn seeded_u0_hunk_headers_match_git_all_algorithms() {
    for seed in 0..1000u32 {
        let (old, new) = seeded_u0_text_pair(seed);
        if old == new {
            continue;
        }
        for algo in [
            LineDiffAlgo::Myers,
            LineDiffAlgo::Minimal,
            LineDiffAlgo::Patience,
            LineDiffAlgo::Histogram,
        ] {
            for indent_on in [false, true] {
                let git_h = git_u0_hunk_headers(&old, &new, algo, indent_on);
                let grit_h = grit_u0_hunk_headers(&old, &new, algo_from_line(algo), indent_on);
                assert_eq!(
                    grit_h, git_h,
                    "seed={seed} algo={algo:?} indent={indent_on}"
                );
            }
        }
    }
}

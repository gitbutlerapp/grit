//! Structured hunk boundaries vs system `git diff` for each line algorithm.

mod support;

use grit_lib::diff::structured::structured_text_diff;
use grit_lib::diff::{DiffAlgorithm, DiffOptions};
use support::{blob_pairs, LineDiffAlgo};

fn blob_text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf8 fixture")
}

fn parse_hunk_headers(patch: &str) -> Vec<(usize, usize, usize, usize)> {
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

fn parse_range(s: &str) -> (usize, usize) {
    if let Some((a, b)) = s.split_once(',') {
        (a.parse().expect("start"), b.parse().expect("count"))
    } else {
        (s.parse().expect("start"), 1)
    }
}

fn git_diff_blob(old: &str, new: &str, algo: LineDiffAlgo, indent: bool, context: usize) -> String {
    use std::process::Command;
    let tmp = tempfile::tempdir().expect("tempdir");
    let old_path = tmp.path().join("old.txt");
    let new_path = tmp.path().join("new.txt");
    std::fs::write(&old_path, old).expect("write old");
    std::fs::write(&new_path, new).expect("write new");
    let mut args = vec![
        "diff".to_owned(),
        "--no-index".to_owned(),
        format!("-U{context}"),
    ];
    match algo {
        LineDiffAlgo::Myers => {}
        LineDiffAlgo::Histogram => args.push("--histogram".to_owned()),
        LineDiffAlgo::Patience => args.push("--patience".to_owned()),
        LineDiffAlgo::Minimal => args.push("--minimal".to_owned()),
    }
    if indent {
        args.push("--indent-heuristic".to_owned());
    } else {
        args.push("--no-indent-heuristic".to_owned());
    }
    args.push(old_path.display().to_string());
    args.push(new_path.display().to_string());
    let out = Command::new("git")
        .args(&args)
        .output()
        .expect("git diff --no-index");
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

fn assert_all_hunks_match_git(
    old: &str,
    new: &str,
    algo: LineDiffAlgo,
    indent_on: bool,
    context: usize,
) {
    let patch = git_diff_blob(old, new, algo, indent_on, context);
    let git_hunks = parse_hunk_headers(&patch);
    assert!(!git_hunks.is_empty(), "expected git hunks");
    let d = structured_text_diff(
        old,
        new,
        DiffOptions::default()
            .with_context_lines(context)
            .with_word_level_inline(false)
            .with_algorithm(algo_from_line(algo))
            .with_indent_heuristic(indent_on),
    );
    let grit_hunks: Vec<_> = d
        .hunks
        .iter()
        .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
        .collect();
    assert_eq!(
        grit_hunks, git_hunks,
        "algo={algo:?} indent={indent_on} context={context}"
    );
}

#[test]
fn mixed_layout_u0_histogram_hunk_headers_match_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4033/t4050-mixed-layout")
        .expect("mixed-layout fixture");
    let old = blob_text(pair.old);
    let new = blob_text(pair.new);
    for indent_on in [false, true] {
        assert_all_hunks_match_git(old, new, LineDiffAlgo::Histogram, indent_on, 0);
    }
}

#[test]
fn t4071_minimal_u0_hunk_headers_match_git() {
    let pairs = blob_pairs();
    let pair = pairs
        .iter()
        .find(|p| p.name == "t4071-minimal")
        .expect("t4071-minimal fixture");
    let old = blob_text(pair.old);
    let new = blob_text(pair.new);
    for indent_on in [false, true] {
        assert_all_hunks_match_git(old, new, LineDiffAlgo::Minimal, indent_on, 0);
    }
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
            assert_all_hunks_match_git(old_p, new_p, algo, indent_on, 3);
        }
    }

    for indent_on in [false, true] {
        assert_all_hunks_match_git(old_i, new_i, LineDiffAlgo::Myers, indent_on, 3);
    }

    for pair in &pairs {
        let old = blob_text(pair.old);
        let new = blob_text(pair.new);
        if old == new {
            continue;
        }
        for algo in [
            LineDiffAlgo::Myers,
            LineDiffAlgo::Minimal,
            LineDiffAlgo::Patience,
            LineDiffAlgo::Histogram,
        ] {
            let patch = git_diff_blob(old, new, algo, true, 3);
            if parse_hunk_headers(&patch).is_empty() {
                continue;
            }
            assert_all_hunks_match_git(old, new, algo, true, 3);
        }
    }
}

#[test]
fn zero_context_deletion_anchor_matches_git_review_case() {
    let old = "  x\n  y\n\n}\n  x\nthree\na\nb\nc\nthree\n\n";
    let new = "  x\n  y\n";
    for algo in [
        LineDiffAlgo::Myers,
        LineDiffAlgo::Minimal,
        LineDiffAlgo::Patience,
        LineDiffAlgo::Histogram,
    ] {
        assert_all_hunks_match_git(old, new, algo, true, 0);
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

fn seeded_text_pair(seed: u64) -> (String, String) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    let h = hasher.finish();

    let line_count = 4 + (h as usize % 12);
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..line_count {
        let token = (h.wrapping_add(i as u64 * 131) % 7) as u8 + b'a';
        let ch = token as char;
        old.push(ch);
        if (h >> (i % 8)) & 1 == 1 {
            new.push(((token as u8).wrapping_add(1)) as char);
        } else {
            new.push(ch);
        }
        old.push('\n');
        new.push('\n');
    }
    (old, new)
}

fn git_diff_u0_text(old: &str, new: &str, algo: LineDiffAlgo, indent: bool) -> String {
    git_diff_blob(old, new, algo, indent, 0)
}

#[test]
fn seeded_u0_histogram_seed_11_matches_git() {
    let (old, new) = seeded_text_pair(11);
    assert_all_hunks_match_git(&old, &new, LineDiffAlgo::Histogram, false, 0);
    assert_all_hunks_match_git(&old, &new, LineDiffAlgo::Histogram, true, 0);
}

#[test]
fn seeded_u0_hunk_headers_match_git_all_algorithms() {
    for seed in 0..1000u64 {
        let (old, new) = seeded_text_pair(seed);
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
                let patch = git_diff_u0_text(&old, &new, algo, indent_on);
                let git_hunks = parse_hunk_headers(&patch);
                if git_hunks.is_empty() {
                    continue;
                }
                let d = structured_text_diff(
                    &old,
                    &new,
                    DiffOptions::default()
                        .with_context_lines(0)
                        .with_word_level_inline(false)
                        .with_algorithm(algo_from_line(algo))
                        .with_indent_heuristic(indent_on),
                );
                let grit_hunks: Vec<_> = d
                    .hunks
                    .iter()
                    .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
                    .collect();
                assert_eq!(
                    grit_hunks, git_hunks,
                    "seed={seed} algo={algo:?} indent={indent_on}"
                );
            }
        }
    }
}

#[test]
fn blob_pairs_u0_complete_hunk_headers_match_git() {
    let pairs = blob_pairs();
    for pair in pairs {
        if pair.name.contains("binary") {
            continue;
        }
        let old = blob_text(pair.old);
        let new = blob_text(pair.new);
        for algo in [
            LineDiffAlgo::Myers,
            LineDiffAlgo::Minimal,
            LineDiffAlgo::Patience,
            LineDiffAlgo::Histogram,
        ] {
            for indent_on in [false, true] {
                let patch = git_diff_blob(&old, &new, algo, indent_on, 0);
                let git_hunks = parse_hunk_headers(&patch);
                if git_hunks.is_empty() && old == new {
                    continue;
                }
                let d = structured_text_diff(
                    &old,
                    &new,
                    DiffOptions::default()
                        .with_context_lines(0)
                        .with_word_level_inline(false)
                        .with_algorithm(algo_from_line(algo))
                        .with_indent_heuristic(indent_on),
                );
                let grit_hunks: Vec<_> = d
                    .hunks
                    .iter()
                    .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
                    .collect();
                assert_eq!(
                    grit_hunks, git_hunks,
                    "pair={} algo={algo:?} indent={indent_on}",
                    pair.name
                );
            }
        }
    }
}

#[test]
fn blob_pairs_u3_histogram_hunk_headers_match_git() {
    let pairs = blob_pairs();
    for pair in pairs {
        if pair.name.contains("binary")
            || pair.name.contains("mixed-layout")
            || pair.name.contains("t4074")
            || pair.name.contains("minimal")
            || pair.name.contains("t4061")
        {
            continue;
        }
        let old = blob_text(pair.old);
        let new = blob_text(pair.new);
        let patch = git_diff_blob(&old, &new, LineDiffAlgo::Histogram, false, 3);
        let git_hunks = parse_hunk_headers(&patch);
        let d = structured_text_diff(
            &old,
            &new,
            DiffOptions::default()
                .with_context_lines(3)
                .with_word_level_inline(false)
                .with_algorithm(DiffAlgorithm::Histogram),
        );
        let grit_hunks: Vec<_> = d
            .hunks
            .iter()
            .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
            .collect();
        assert_eq!(grit_hunks, git_hunks, "pair={}", pair.name);
    }
}

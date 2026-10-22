//! Blob-level patch apply engine: unit, property, and git apply compat tests.

use std::process::Command;

use grit_lib::apply::{apply_hunks_to_blob, parse_patch, ApplyOptions, Hunk, HunkLine};
use grit_lib::diff::structured::{structured_text_diff, StructuredHunk, StructuredLineKind};
use grit_lib::diff::DiffOptions;
use grit_lib::error::ApplyError;
use tempfile::TempDir;

type TestResult = Result<(), String>;

fn tempdir() -> Result<TempDir, String> {
    TempDir::new().map_err(|e| format!("tempdir: {e}"))
}

fn hunks_from_git_diff(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, String> {
    let dir = tempdir()?;
    let old_path = dir.path().join("old.txt");
    let new_path = dir.path().join("new.txt");
    std::fs::write(&old_path, old).map_err(|e| format!("write old: {e}"))?;
    std::fs::write(&new_path, new).map_err(|e| format!("write new: {e}"))?;
    let out = Command::new("git")
        .args([
            "diff",
            "--no-index",
            old_path.to_str().ok_or("old path utf8")?,
            new_path.to_str().ok_or("new path utf8")?,
        ])
        .output()
        .map_err(|e| format!("git diff: {e}"))?;
    let patch = String::from_utf8_lossy(&out.stdout);
    let files = parse_patch(&patch, 1, "patch", false, None).map_err(|e| format!("parse: {e}"))?;
    Ok(files.into_iter().next().map(|f| f.hunks).unwrap_or_default())
}

fn patch_text_from_hunks(hunks: &[Hunk]) -> String {
    let mut out = String::from("--- a/f\n+++ b/f\n");
    for h in hunks {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_hunk_range(h.old_start, h.old_count),
            format_hunk_range(h.new_start, h.new_count),
        ));
        for line in &h.lines {
            match line {
                HunkLine::Context(s) => {
                    out.push(' ');
                    out.push_str(s);
                    out.push('\n');
                }
                HunkLine::Add(s) => {
                    out.push('+');
                    out.push_str(s);
                    out.push('\n');
                }
                HunkLine::Remove(s) => {
                    out.push('-');
                    out.push_str(s);
                    out.push('\n');
                }
                HunkLine::NoNewline => {
                    out.push_str("\\ No newline at end of file\n");
                }
            }
        }
    }
    out
}

fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 0 {
        format!("{start},0")
    } else if count == 1 {
        format!("{start}")
    } else {
        format!("{start},{count}")
    }
}

fn git_apply_bytes(original: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>, String> {
    let patch = patch_text_from_hunks(hunks);
    let dir = tempdir()?;
    let file = dir.path().join("f");
    std::fs::write(&file, original).map_err(|e| format!("write file: {e}"))?;
    let patch_path = dir.path().join("p.patch");
    std::fs::write(&patch_path, &patch).map_err(|e| format!("write patch: {e}"))?;
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["apply", patch_path.to_str().ok_or("patch path utf8")?])
        .status()
        .map_err(|e| format!("git apply: {e}"))?;
    if !status.success() {
        return Err(format!("git apply failed: {patch}"));
    }
    std::fs::read(&file).map_err(|e| format!("read result: {e}"))
}

fn grit_apply_bytes(original: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>, String> {
    apply_hunks_to_blob(original, hunks, &ApplyOptions::default()).map_err(|e| format!("{e}"))
}

fn run_test(result: TestResult) {
    if let Err(e) = result {
        panic!("{e}");
    }
}

#[test]
fn apply_exact_match() {
    run_test(apply_exact_match_impl());
}

fn apply_exact_match_impl() -> TestResult {
    let old = b"alpha\nbeta\ngamma\n";
    let new = b"alpha\nBETA\ngamma\n";
    let hunks = hunks_from_git_diff(old, new)?;
    assert_eq!(grit_apply_bytes(old, &hunks)?, new);
    Ok(())
}

#[test]
fn apply_with_offset() {
    run_test(apply_with_offset_impl());
}

fn apply_with_offset_impl() -> TestResult {
    let old = b"keep\nalpha\nbeta\ngamma\ntail\n";
    let new = b"keep\nalpha\nBETA\ngamma\ntail\n";
    let mut hunks = hunks_from_git_diff(b"alpha\nbeta\ngamma\n", b"alpha\nBETA\ngamma\n")?;
    hunks[0].old_start = 1;
    assert_eq!(grit_apply_bytes(old, &hunks)?, new);
    Ok(())
}

#[test]
fn apply_rejects_context_mismatch() {
    run_test(apply_rejects_context_mismatch_impl());
}

fn apply_rejects_context_mismatch_impl() -> TestResult {
    let old = b"one\ntwo\n";
    let hunks = vec![Hunk {
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
        first_body_line: 2,
        lines: vec![
            HunkLine::Remove("NOT-IN-FILE".to_string()),
            HunkLine::Add("x".to_string()),
        ],
    }];
    match apply_hunks_to_blob(old, &hunks, &ApplyOptions::default()) {
        Err(ApplyError::ContextMismatch { .. }) | Err(ApplyError::UnexpectedEof { .. }) => Ok(()),
        Err(other) => Err(format!("unexpected error: {other}")),
        Ok(_) => Err("expected apply failure".to_string()),
    }
}

#[test]
fn apply_no_newline_both_sides() {
    run_test(apply_no_newline_both_sides_impl());
}

fn apply_no_newline_both_sides_impl() -> TestResult {
    let old = b"no newline";
    let new = b"yes newline\n";
    let hunks = hunks_from_git_diff(old, new)?;
    assert_eq!(grit_apply_bytes(old, &hunks)?, new);
    let reverse = hunks_from_git_diff(new, old)?;
    let back = apply_hunks_to_blob(new, &reverse, &ApplyOptions::default())
        .map_err(|e| format!("reverse: {e}"))?;
    assert_eq!(back, old);
    Ok(())
}

#[test]
fn apply_boundary_first_and_last_line() {
    run_test(apply_boundary_first_and_last_line_impl());
}

fn apply_boundary_first_and_last_line_impl() -> TestResult {
    let old = b"first\nmiddle\nlast";
    let new = b"FIRST\nmiddle\nLAST";
    let hunks = hunks_from_git_diff(old, new)?;
    assert_eq!(grit_apply_bytes(old, &hunks)?, new);
    Ok(())
}

#[test]
fn apply_reverse_roundtrip() {
    run_test(apply_reverse_roundtrip_impl());
}

fn apply_reverse_roundtrip_impl() -> TestResult {
    let old = b"a\nb\nc\n";
    let new = b"a\nB\nc\nd\n";
    let forward = hunks_from_git_diff(old, new)?;
    let mid = grit_apply_bytes(old, &forward)?;
    assert_eq!(mid, new);
    let back = apply_hunks_to_blob(&mid, &forward, &ApplyOptions { reverse: true })
        .map_err(|e| format!("reverse apply: {e}"))?;
    assert_eq!(back, old);
    Ok(())
}

#[test]
fn apply_empty_file_creation_and_deletion() {
    run_test(apply_empty_file_creation_and_deletion_impl());
}

fn apply_empty_file_creation_and_deletion_impl() -> TestResult {
    let added = hunks_from_git_diff(b"", b"new\n")?;
    assert_eq!(grit_apply_bytes(b"", &added)?, b"new\n");
    let deleted = hunks_from_git_diff(b"x\n", b"")?;
    assert_eq!(grit_apply_bytes(b"x\n", &deleted)?, b"");
    Ok(())
}

#[test]
fn apply_multi_hunk_duplicate_context_uses_new_start() {
    run_test(apply_multi_hunk_duplicate_context_uses_new_start_impl());
}

fn apply_multi_hunk_duplicate_context_uses_new_start_impl() -> TestResult {
    let mut old = String::new();
    for i in 1..=9 {
        use std::fmt::Write as _;
        writeln!(old, "filler-{i:02}").map_err(|e| format!("fmt: {e}"))?;
    }
    old.push_str("target/context\n");
    for i in 10..=19 {
        use std::fmt::Write as _;
        writeln!(old, "gap-{i}").map_err(|e| format!("fmt: {e}"))?;
    }
    old.push_str("target/context\n");

    let mut new = String::new();
    for i in 1..=9 {
        use std::fmt::Write as _;
        writeln!(new, "filler-{i:02}").map_err(|e| format!("fmt: {e}"))?;
        if i == 5 {
            for j in 0..10 {
                writeln!(new, "insert-{j:02}").map_err(|e| format!("fmt: {e}"))?;
            }
        }
    }
    new.push_str("target/context\n");
    for i in 10..=19 {
        use std::fmt::Write as _;
        writeln!(new, "gap-{i}").map_err(|e| format!("fmt: {e}"))?;
    }
    new.push_str("target/CHANGED\n");

    let old_b = old.as_bytes();
    let new_b = new.as_bytes();
    let hunks = hunks_from_git_diff(old_b, new_b)?;
    assert!(
        hunks.len() >= 2,
        "expected multi-hunk patch, got {} hunks",
        hunks.len()
    );

    let grit_out = grit_apply_bytes(old_b, &hunks)?;
    let git_out = git_apply_bytes(old_b, &hunks)?;
    assert_eq!(grit_out, git_out);

    let grit_text = String::from_utf8_lossy(&grit_out);
    let first_row = grit_text
        .lines()
        .filter(|l| *l == "target/context")
        .count();
    assert_eq!(first_row, 1, "first duplicate must stay intact");
    assert!(
        grit_text.contains("target/CHANGED"),
        "second duplicate must be updated"
    );
    Ok(())
}

#[test]
fn apply_structured_text_diff_property_sample() {
    run_test(apply_structured_text_diff_property_sample_impl());
}

fn apply_structured_text_diff_property_sample_impl() -> TestResult {
    for (old, new) in [
        ("", "hello\n"),
        ("abc\n", "abc\nxyz\n"),
        ("line\n", "line"),
        ("a\nb\n", "b\nc\n"),
    ] {
        structured_roundtrip(old, new)?;
    }
    Ok(())
}

#[test]
fn apply_structured_text_diff_deterministic_pairs() {
    run_test(apply_structured_text_diff_deterministic_pairs_impl());
}

fn apply_structured_text_diff_deterministic_pairs_impl() -> TestResult {
    for seed in 0u32..48 {
        let old = det_text(seed, 40 + (seed % 20) as usize);
        let new = det_text(seed.wrapping_mul(31), 35 + (seed % 25) as usize);
        structured_roundtrip(&old, &new)?;
    }
    Ok(())
}

fn det_text(seed: u32, len: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz \n";
    let mut s = String::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let idx = (x as usize) % alphabet.len();
        s.push(char::from(alphabet[idx]));
    }
    s
}

fn structured_roundtrip(old: &str, new: &str) -> TestResult {
    let diff = structured_text_diff(old, new, DiffOptions::default());
    let patch = diff_to_patch(&diff.hunks);
    let hunks = parse_patch(&patch, 1, "p", false, None)
        .map_err(|e| format!("parse: {e}"))?
        .pop()
        .ok_or("missing file patch")?
        .hunks;
    let got = apply_hunks_to_blob(old.as_bytes(), &hunks, &ApplyOptions::default())
        .map_err(|e| format!("forward apply: {e}"))?;
    if got != new.as_bytes() {
        return Err(format!(
            "forward mismatch old={old:?} new={new:?} got={got:?}"
        ));
    }
    let rev = apply_hunks_to_blob(new.as_bytes(), &hunks, &ApplyOptions { reverse: true })
        .map_err(|e| format!("reverse apply: {e}"))?;
    if rev != old.as_bytes() {
        return Err(format!(
            "reverse mismatch old={old:?} new={new:?} rev={rev:?}"
        ));
    }
    Ok(())
}

fn diff_to_patch(hunks: &[StructuredHunk]) -> String {
    let mut out = String::from("--- a/f\n+++ b/f\n");
    for h in hunks {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_hunk_range(h.old_start, h.old_lines),
            format_hunk_range(h.new_start, h.new_lines),
        ));
        for line in &h.lines {
            let prefix = match line.kind {
                StructuredLineKind::Context => ' ',
                StructuredLineKind::Add => '+',
                StructuredLineKind::Del => '-',
            };
            let text = line
                .segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>();
            if prefix == ' ' && text.is_empty() {
                out.push('\n');
            } else {
                out.push(prefix);
                out.push_str(&text);
                out.push('\n');
            }
            if line.no_newline_at_eof {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

#[test]
fn apply_git_apply_compat_corpus() {
    run_test(apply_git_apply_compat_corpus_impl());
}

fn apply_git_apply_compat_corpus_impl() -> TestResult {
    let cases: Vec<(&[u8], &[u8])> = vec![
        (b"one\n", b"two\n"),
        (b"a\nb\nc\n", b"a\nB\nc\nd\n"),
        (b"start", b"end"),
        (b"x\n\ny\n", b"x\nY\n\nz\n"),
    ];
    for (old, new) in cases {
        let dir = tempdir()?;
        let old_p = dir.path().join("o");
        let new_p = dir.path().join("n");
        std::fs::write(&old_p, old).map_err(|e| format!("write o: {e}"))?;
        std::fs::write(&new_p, new).map_err(|e| format!("write n: {e}"))?;
        let out = Command::new("git")
            .args([
                "diff",
                "--no-index",
                old_p.to_str().ok_or("o utf8")?,
                new_p.to_str().ok_or("n utf8")?,
            ])
            .output()
            .map_err(|e| format!("git diff: {e}"))?;
        let hunks = parse_patch(&String::from_utf8_lossy(&out.stdout), 1, "p", false, None)
            .map_err(|e| format!("parse: {e}"))?
            .pop()
            .ok_or("file")?
            .hunks;
        let grit_out = grit_apply_bytes(old, &hunks)?;
        let git_out = git_apply_bytes(old, &hunks)?;
        if grit_out != git_out {
            return Err(format!("mismatch for old={old:?}"));
        }
    }
    Ok(())
}

#[test]
fn apply_trailing_context_beyond_hint_matches_with_offset() {
    run_test(apply_trailing_context_beyond_hint_matches_with_offset_impl());
}

fn apply_trailing_context_beyond_hint_matches_with_offset_impl() -> TestResult {
    let old = b"a\nb\nc\nd\n";
    let patch = "--- a/f\n+++ b/f\n@@ -10,2 +10,2 @@\n-c\n+C\n d\n";
    let hunks = parse_patch(patch, 1, "p", false, None)
        .map_err(|e| format!("parse: {e}"))?
        .pop()
        .ok_or("file")?
        .hunks;
    let grit_out = grit_apply_bytes(old, &hunks)?;
    let git_out = git_apply_bytes(old, &hunks)?;
    assert_eq!(grit_out, git_out, "expected git-compatible offset apply");
    assert_eq!(grit_out, b"a\nb\nC\nd\n");
    Ok(())
}

#[test]
fn apply_no_context_first_line_change() {
    run_test(apply_no_context_first_line_change_impl());
}

fn apply_no_context_first_line_change_impl() -> TestResult {
    let old = b"OLD\nmiddle\nlast\n";
    let new = b"NEW\nmiddle\nlast\n";
    let hunks = hunks_from_git_diff(old, new)?;
    let grit_out = grit_apply_bytes(old, &hunks)?;
    let git_out = git_apply_bytes(old, &hunks)?;
    assert_eq!(grit_out, git_out);
    assert_eq!(grit_out, new);
    Ok(())
}

#[test]
fn apply_append_lines_at_eof() {
    run_test(apply_append_lines_at_eof_impl());
}

fn apply_append_lines_at_eof_impl() -> TestResult {
    let old = b"one\ntwo\nthree\n";
    let new = b"one\ntwo\nthree\nfour\nfive\n";
    let hunks = hunks_from_git_diff(old, new)?;
    let grit_out = grit_apply_bytes(old, &hunks)?;
    let git_out = git_apply_bytes(old, &hunks)?;
    assert_eq!(grit_out, git_out);
    assert_eq!(grit_out, new);
    Ok(())
}

#[test]
fn apply_deterministic_git_diff_pairs_match_git() {
    run_test(apply_deterministic_git_diff_pairs_match_git_impl());
}

fn apply_deterministic_git_diff_pairs_match_git_impl() -> TestResult {
    for seed in 0u32..32 {
        let old_raw = det_bytes(seed, 80 + (seed % 40) as usize);
        let new_raw = det_bytes(seed.wrapping_add(17), 70 + (seed % 50) as usize);
        let hunks = hunks_from_git_diff(&old_raw, &new_raw)?;
        if hunks.is_empty() && old_raw == new_raw {
            continue;
        }
        let grit_out = grit_apply_bytes(&old_raw, &hunks)?;
        let git_out = git_apply_bytes(&old_raw, &hunks)?;
        if grit_out != git_out {
            return Err(format!("seed={seed} mismatch"));
        }
    }
    Ok(())
}

fn det_bytes(seed: u32, len: usize) -> Vec<u8> {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789\n";
    let mut v = Vec::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push(alphabet[(x as usize) % alphabet.len()]);
    }
    v
}

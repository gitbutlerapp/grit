//! Blob-level patch apply engine: unit, property, and git apply compat tests.

use std::process::Command;

use grit_lib::apply::{apply_hunks_to_blob, parse_patch, ApplyOptions, Hunk, HunkLine};
use grit_lib::diff::structured::{structured_text_diff, StructuredHunk, StructuredLineKind};
use grit_lib::diff::DiffOptions;
use grit_lib::error::ApplyError;
use tempfile::TempDir;

fn hunks_from_git_diff(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    let dir = TempDir::new().expect("tempdir");
    let old_path = dir.path().join("old.txt");
    let new_path = dir.path().join("new.txt");
    std::fs::write(&old_path, old).expect("write old");
    std::fs::write(&new_path, new).expect("write new");
    let out = Command::new("git")
        .args([
            "diff",
            "--no-index",
            old_path.to_str().expect("utf8"),
            new_path.to_str().expect("utf8"),
        ])
        .output()
        .expect("git diff");
    let patch = String::from_utf8_lossy(&out.stdout);
    let files = parse_patch(&patch, 1, "patch", false, None).expect("parse");
    files
        .into_iter()
        .next()
        .map(|f| f.hunks)
        .unwrap_or_default()
}

fn patch_text_from_hunks(hunks: &[Hunk]) -> String {
    let mut out = String::from("--- a/f\n+++ b/f\n");
    for h in hunks {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_hunk_range(h.old_start, h.old_count),
            format_hunk_range(h.new_start, h.new_count),
        ));
        let mut i = 0usize;
        while i < h.lines.len() {
            match &h.lines[i] {
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
            i += 1;
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

fn git_apply_bytes(original: &[u8], hunks: &[Hunk]) -> Vec<u8> {
    let patch = patch_text_from_hunks(hunks);
    let dir = TempDir::new().expect("tempdir");
    let file = dir.path().join("f");
    std::fs::write(&file, original).expect("write");
    let patch_path = dir.path().join("p.patch");
    std::fs::write(&patch_path, &patch).expect("patch");
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["apply", patch_path.to_str().expect("utf8")])
        .status()
        .expect("git apply");
    assert!(status.success(), "git apply failed: {patch}");
    std::fs::read(&file).expect("read result")
}

fn grit_apply_bytes(original: &[u8], hunks: &[Hunk]) -> Vec<u8> {
    apply_hunks_to_blob(original, hunks, &ApplyOptions::default()).expect("apply")
}

#[test]
fn apply_exact_match() {
    let old = b"alpha\nbeta\ngamma\n";
    let new = b"alpha\nBETA\ngamma\n";
    let hunks = hunks_from_git_diff(old, new);
    assert_eq!(grit_apply_bytes(old, &hunks), new);
}

#[test]
fn apply_with_offset() {
    let old = b"keep\nalpha\nbeta\ngamma\ntail\n";
    let new = b"keep\nalpha\nBETA\ngamma\ntail\n";
    let mut hunks = hunks_from_git_diff(b"alpha\nbeta\ngamma\n", b"alpha\nBETA\ngamma\n");
    // Shift hunk to wrong line; context should still match with offset search.
    hunks[0].old_start = 1;
    assert_eq!(grit_apply_bytes(old, &hunks), new);
}

#[test]
fn apply_rejects_context_mismatch() {
    let old = b"one\ntwo\n";
    let hunks = vec![Hunk {
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
        first_body_line: 2,
        lines: vec![
            HunkLine::Remove("one".to_string()),
            HunkLine::Add("ONE".to_string()),
        ],
    }];
    let err = apply_hunks_to_blob(old, &hunks, &ApplyOptions::default()).unwrap_err();
    assert!(matches!(err, ApplyError::ContextMismatch { .. }));
}

#[test]
fn apply_no_newline_both_sides() {
    let old = b"no newline";
    let new = b"yes newline\n";
    let hunks = hunks_from_git_diff(old, new);
    assert_eq!(grit_apply_bytes(old, &hunks), new);
    let reverse = hunks_from_git_diff(new, old);
    let back = apply_hunks_to_blob(new, &reverse, &ApplyOptions::default()).expect("reverse");
    assert_eq!(back, old);
}

#[test]
fn apply_boundary_first_and_last_line() {
    let old = b"first\nmiddle\nlast";
    let new = b"FIRST\nmiddle\nLAST";
    let hunks = hunks_from_git_diff(old, new);
    assert_eq!(grit_apply_bytes(old, &hunks), new);
}

#[test]
fn apply_reverse_roundtrip() {
    let old = b"a\nb\nc\n";
    let new = b"a\nB\nc\nd\n";
    let forward = hunks_from_git_diff(old, new);
    let mid = grit_apply_bytes(old, &forward);
    assert_eq!(mid, new);
    let back = apply_hunks_to_blob(&mid, &forward, &ApplyOptions { reverse: true })
        .expect("reverse apply");
    assert_eq!(back, old);
}

#[test]
fn apply_empty_file_creation_and_deletion() {
    let added = hunks_from_git_diff(b"", b"new\n");
    assert_eq!(grit_apply_bytes(b"", &added), b"new\n");
    let deleted = hunks_from_git_diff(b"x\n", b"");
    assert_eq!(grit_apply_bytes(b"x\n", &deleted), b"");
}

#[test]
fn apply_structured_text_diff_property_sample() {
    for (old, new) in [
        ("", "hello\n"),
        ("abc\n", "abc\nxyz\n"),
        ("line\n", "line"),
        ("a\nb\n", "b\nc\n"),
    ] {
        let diff = structured_text_diff(old, new, DiffOptions::default());
        let patch = diff_to_patch(old, new, &diff.hunks);
        let hunks = parse_patch(&patch, 1, "p", false, None)
            .expect("parse")
            .pop()
            .expect("one file")
            .hunks;
        let got = apply_hunks_to_blob(old.as_bytes(), &hunks, &ApplyOptions::default())
            .expect("apply forward");
        assert_eq!(got, new.as_bytes(), "old={old:?} new={new:?}");
        let rev = apply_hunks_to_blob(new.as_bytes(), &hunks, &ApplyOptions { reverse: true })
            .expect("apply reverse");
        assert_eq!(rev, old.as_bytes());
    }
}

fn diff_to_patch(old: &str, new: &str, hunks: &[StructuredHunk]) -> String {
    let mut out = format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n");
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
    let _ = (old, new);
    out
}

#[test]
fn apply_git_apply_compat_corpus() {
    let cases: Vec<(&[u8], &[u8])> = vec![
        (b"one\n", b"two\n"),
        (b"a\nb\nc\n", b"a\nB\nc\nd\n"),
        (b"start", b"end"),
        (b"x\n\ny\n", b"x\nY\n\nz\n"),
    ];
    for (old, new) in cases {
        let dir = TempDir::new().expect("tempdir");
        let old_p = dir.path().join("o");
        let new_p = dir.path().join("n");
        std::fs::write(&old_p, old).unwrap();
        std::fs::write(&new_p, new).unwrap();
        let out = Command::new("git")
            .args([
                "diff",
                "--no-index",
                old_p.to_str().unwrap(),
                new_p.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let hunks = parse_patch(&String::from_utf8_lossy(&out.stdout), 1, "p", false, None)
            .unwrap()
            .pop()
            .unwrap()
            .hunks;
        let grit_out = grit_apply_bytes(old, &hunks);
        let git_out = git_apply_bytes(old, &hunks);
        assert_eq!(grit_out, git_out, "mismatch for old={old:?}");
    }
}

#[test]
fn apply_random_text_pairs_match_git() {
    use getrandom::getrandom;

    for seed in 0u32..32 {
        let mut buf = [0u8; 8];
        getrandom(&mut buf).expect("random");
        let len_old = (u16::from_le_bytes([buf[0], buf[1]]) % 120) as usize;
        let len_new = (u16::from_le_bytes([buf[2], buf[3]]) % 120) as usize;
        let mut old_raw = vec![0u8; len_old];
        let mut new_raw = vec![0u8; len_new];
        getrandom(&mut old_raw).ok();
        getrandom(&mut new_raw).ok();
        for b in old_raw.iter_mut().chain(new_raw.iter_mut()) {
            *b = b"abcdefghijklmnopqrstuvwxyz\n\r"[(*b as usize) % 28];
        }
        if seed % 3 == 0 {
            old_raw.retain(|&c| c != b'\r');
            new_raw.retain(|&c| c != b'\r');
        }
        let hunks = hunks_from_git_diff(&old_raw, &new_raw);
        if hunks.is_empty() && old_raw == new_raw {
            continue;
        }
        let grit_out = match apply_hunks_to_blob(&old_raw, &hunks, &ApplyOptions::default()) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("o"), &old_raw).unwrap();
        std::fs::write(dir.path().join("n"), &new_raw).unwrap();
        let diff = Command::new("git")
            .args([
                "diff",
                "--no-index",
                dir.path().join("o").to_str().unwrap(),
                dir.path().join("n").to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let git_result = git_apply_bytes(&old_raw, &hunks);
        assert_eq!(grit_out, git_result, "seed={seed}");
    }
}

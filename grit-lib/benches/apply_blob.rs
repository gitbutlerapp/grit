//! Criterion bench for blob-level hunk application.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::apply::{apply_hunks_to_blob, parse_patch, ApplyOptions, Hunk};

fn fifty_k_line_blob() -> Vec<u8> {
    let mut out = Vec::with_capacity(50_000 * 16);
    for i in 0..50_000 {
        use std::fmt::Write as _;
        let mut line = String::new();
        let _ = writeln!(line, "line-{i:06}-padding");
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn five_hundred_hunks_on_blob(base: &[u8]) -> Vec<Hunk> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    for (i, &b) in base.iter().enumerate() {
        if b == b'\n' {
            lines.push(&base[start..=i]);
            start = i + 1;
        }
    }
    if start < base.len() {
        lines.push(&base[start..]);
    }

    let mut hunks = Vec::with_capacity(500);
    for n in 0..500 {
        let line_idx = n * 100 + 10;
        if line_idx >= lines.len() {
            break;
        }
        let old_line = lines[line_idx];
        let old_body = old_line.strip_suffix(b"\n").unwrap_or(old_line);
        let mut new_body = old_body.to_vec();
        new_body.extend_from_slice(b" edited");
        if old_line.ends_with(b"\n") {
            new_body.push(b'\n');
        }
        hunks.push(Hunk {
            old_start: line_idx + 1,
            old_count: 1,
            new_start: line_idx + 1,
            new_count: 1,
            first_body_line: 0,
            lines: vec![
                grit_lib::apply::HunkLine::Remove(String::from_utf8_lossy(old_body).into_owned()),
                grit_lib::apply::HunkLine::Add(String::from_utf8_lossy(&new_body).into_owned()),
            ],
        });
    }
    hunks
}

fn bench_apply_hunks_500_on_50k_lines(c: &mut Criterion) {
    let base = fifty_k_line_blob();
    let hunks = five_hundred_hunks_on_blob(&base);
    c.bench_function("apply_hunks_500_on_50k_lines", |b| {
        b.iter(|| {
            black_box(apply_hunks_to_blob(
                black_box(&base),
                black_box(&hunks),
                black_box(&ApplyOptions::default()),
            ))
        });
    });
}

fn bench_parse_patch_sample(c: &mut Criterion) {
    let base = fifty_k_line_blob();
    let mut patch = String::from("--- a/f\n+++ b/f\n");
    patch.push_str("@@ -11 +11 @@\n");
    patch.push_str(" line-000010-padding\n");
    patch.push_str("-line-000010-padding\n");
    patch.push_str("+line-000010-padding edited\n");
    c.bench_function("parse_patch_small", |b| {
        b.iter(|| black_box(parse_patch(black_box(&patch), 1, "p", false, None)));
    });
    let _ = base;
}

criterion_group!(
    apply_blob_benches,
    bench_apply_hunks_500_on_50k_lines,
    bench_parse_patch_sample
);
criterion_main!(apply_blob_benches);

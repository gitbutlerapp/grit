//! Count-only line diffs used by log/stat, including large files with small edits.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use grit_lib::diff::{count_changes_with_algorithm, line_change_counts, DiffOptions};

fn line_stats(c: &mut Criterion) {
    let options = DiffOptions::default();
    let mut group = c.benchmark_group("log_stat");
    for (name, old, new) in [
        {
            let prefix = "unchanged prefix\n".repeat(50_000);
            let suffix = "unchanged suffix\n".repeat(50_000);
            (
                "100k_lines_small_edit",
                format!("{prefix}old\n{suffix}"),
                format!("{prefix}new\nextra\n{suffix}"),
            )
        },
        (
            "1k_lines_replaced",
            "old\n".repeat(1000),
            "new\n".repeat(1000),
        ),
    ] {
        group.bench_with_input(
            BenchmarkId::new("full_file", name),
            &(&old, &new),
            |b, (old, new)| {
                b.iter(|| {
                    black_box(count_changes_with_algorithm(
                        black_box(old),
                        black_box(new),
                        similar::Algorithm::Myers,
                        false,
                    ))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("stat", name),
            &(&old, &new),
            |b, (old, new)| {
                b.iter(|| black_box(line_change_counts(black_box(old), black_box(new), &options)));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, line_stats);
criterion_main!(benches);

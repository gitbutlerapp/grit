//! EWAH expand, OR-from-compressed, and popcount (~420k bits, git.git object scale).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};

fn bench_ewah(c: &mut Criterion) {
    let bytes = grit_lib::ewah_bench::git_sized_bytes();

    let mut group = c.benchmark_group("ewah_git_sized");
    group.bench_function("expand", |b| {
        b.iter(|| black_box(grit_lib::ewah_bench::expand_fixture(bytes)));
    });
    group.bench_function("or_into", |b| {
        b.iter(|| black_box(grit_lib::ewah_bench::or_into_fixture()));
    });
    group.bench_function("popcount", |b| {
        b.iter(|| black_box(grit_lib::ewah_bench::popcount_fixture(bytes)));
    });
    group.finish();
}

criterion_group!(benches, bench_ewah);
criterion_main!(benches);

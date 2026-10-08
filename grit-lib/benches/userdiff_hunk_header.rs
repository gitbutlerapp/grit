//! Criterion: built-in userdiff funcname matching (hunk-header hot path).

#![allow(clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::ConfigSet;
use grit_lib::userdiff::matcher_for_driver;

fn bench_rust_funcname_match(c: &mut Criterion) {
    let config = ConfigSet::default();
    let matcher = matcher_for_driver(&config, "rust")
        .expect("matcher")
        .expect("rust builtin");
    let line = "pub fn example() {";
    c.bench_function("userdiff_hunk_header_rust_match_line", |b| {
        b.iter(|| black_box(matcher.match_line(black_box(line)).is_some()));
    });
}

criterion_group!(
    name = userdiff_hunk_header;
    config = Criterion::default().sample_size(50);
    targets = bench_rust_funcname_match
);
criterion_main!(userdiff_hunk_header);

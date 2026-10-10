//! Criterion throughput for [`grit_lib::delta_encode::DeltaIndex`].

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use grit_lib::delta_encode::DeltaIndex;

fn text_blob(len: usize) -> Vec<u8> {
    let line = b"The quick brown fox jumps over the lazy dog.\n";
    line.iter().cycle().take(len).copied().collect()
}

fn binary_blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(131) >> 8) as u8).collect()
}

fn edited_text(base: &[u8]) -> Vec<u8> {
    let mut target = base.to_vec();
    let mid = target.len() / 2;
    target.splice(mid..mid, b"// delta bench insert\n".repeat(32));
    target
}

fn bench_delta_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_encode");
    for (label, len) in [("text_256k", 256 * 1024), ("binary_256k", 256 * 1024)] {
        let base = if label.starts_with("text") {
            text_blob(len)
        } else {
            binary_blob(len)
        };
        let target = edited_text(&base);
        let index = DeltaIndex::new(&base);
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("encode", label), &target, |b, target| {
            b.iter(|| black_box(index.encode(target, 0).expect("encode")));
        });
    }
    group.finish();
}

criterion_group!(delta_encode, bench_delta_encode);
criterion_main!(delta_encode);

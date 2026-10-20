//! Side-band pack demux throughput (band-1 only, no HTTP).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::io::Cursor;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use grit_lib::{read_sideband_pack_to, PackReceiveTarget};

use grit_lib::remote::fetch::NoProgress;

fn sideband_band1_stream(pack_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let chunk = 65520usize;
    let mut remaining = pack_bytes;
    let mut first = true;
    while remaining > 0 {
        let payload_len = remaining.min(chunk - 1);
        let data = if first {
            first = false;
            let mut v = b"PACK".to_vec();
            v.extend(std::iter::repeat(b'x').take(payload_len.saturating_sub(4)));
            v.truncate(payload_len);
            v
        } else {
            vec![b'x'; payload_len]
        };
        remaining = remaining.saturating_sub(payload_len);
        let pkt_len = 4 + 1 + data.len();
        out.extend(format!("{pkt_len:04x}").as_bytes());
        out.push(1);
        out.extend_from_slice(&data);
    }
    out.extend_from_slice(b"0000");
    out
}

fn bench_sideband_demux(c: &mut Criterion) {
    let mut group = c.benchmark_group("sideband_demux_band1");
    for size in [8 * 1024 * 1024, 64 * 1024 * 1024] {
        let stream = sideband_band1_stream(size);
        group.throughput(criterion::Throughput::Bytes(size as u64));
        let label = format!("{}MiB", size / (1024 * 1024));
        group.bench_with_input(BenchmarkId::new("band1", &label), &stream, |b, stream| {
            b.iter(|| {
                let mut target_buf = Vec::with_capacity(size);
                let mut target = PackReceiveTarget::Memory(&mut target_buf);
                let mut progress = NoProgress;
                read_sideband_pack_to(
                    &mut Cursor::new(stream.as_slice()),
                    &mut target,
                    &mut progress,
                )
                .expect("demux");
                black_box(target_buf.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sideband_demux);
criterion_main!(benches);

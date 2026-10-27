//! Scattered pack-index probes on a real pack, or a deterministic 200k-object index.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::objects::pack::{read_pack_index_no_verify, write_v2_pack_index_with_trailer};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};

fn bench_pack_index(c: &mut Criterion) {
    let temp = tempfile::tempdir().expect("benchmark directory");
    let path = match std::env::var_os("GRIT_PACK_INDEX_BENCH") {
        Some(path) => PathBuf::from(path),
        None => {
            let path = temp.path().join("lookup.idx");
            let entries: Vec<_> = (0..200_000u32)
                .map(|i| {
                    let oid = HashAlgo::Sha1
                        .hash_object(ObjectKind::Blob, &i.to_be_bytes())
                        .expect("hash");
                    (oid, 12 + u64::from(i) * 40, 0)
                })
                .collect();
            write_v2_pack_index_with_trailer(&path, &entries, &[0; 20], 20, None)
                .expect("write benchmark index");
            path
        }
    };
    let index = read_pack_index_no_verify(&path).expect("read benchmark index");
    assert!(!index.is_empty());
    let hits: Vec<_> = (0..4096u64)
        .map(|i| {
            let row = (i * 2_654_435_761 % index.len() as u64) as usize;
            ObjectId::from_bytes(index.oid_at(row)).expect("oid")
        })
        .collect();
    let misses: Vec<_> = hits
        .iter()
        .map(|oid| {
            let mut bytes = oid.as_bytes().to_vec();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            ObjectId::from_bytes(&bytes).expect("miss oid")
        })
        .filter(|oid| index.find_position(oid).is_none())
        .collect();
    assert!(!misses.is_empty());
    let mut group = c.benchmark_group("pack_idx_lookup");
    for (name, probes) in [("scattered_hits", &hits), ("scattered_misses", &misses)] {
        group.throughput(Throughput::Elements(probes.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                for oid in probes {
                    black_box(index.find_position(black_box(oid)));
                }
            });
        });
    }
    group.throughput(Throughput::Elements(1));
    group.bench_function("single_hit", |b| {
        b.iter(|| black_box(index.find_position(black_box(&hits[0]))));
    });
    group.bench_function("single_miss", |b| {
        b.iter(|| black_box(index.find_position(black_box(&misses[0]))));
    });
    group.finish();
}

criterion_group!(benches, bench_pack_index);
criterion_main!(benches);

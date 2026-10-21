//! Compare SipHash vs [`OidBuildHasher`] for OID-keyed maps at ~1M entries.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use grit_lib::objects::{
    hash::ObjectHasher, oid_hash_map, oid_hash_map_with_capacity, oid_hash_set, HashAlgo, ObjectId,
    OidHashMap, OidHashSet,
};

const COUNT: usize = 1_000_000;

/// Deterministic SHA-1 OIDs with distinct first-eight-byte prefixes (no bucket collapse).
fn sample_ids(count: usize) -> Vec<ObjectId> {
    (0..count)
        .map(|i| {
            let mut hasher = ObjectHasher::new(HashAlgo::Sha1);
            hasher.update(b"grit-lib oid_hashmap bench");
            hasher.update(&i.to_le_bytes());
            hasher.finalize()
        })
        .collect()
}

fn bench_insert_presized(c: &mut Criterion) {
    let ids = sample_ids(COUNT);
    let mut group = c.benchmark_group("oid_hashmap_insert_presized");
    group.sample_size(10);

    group.bench_with_input(BenchmarkId::new("siphash", COUNT), &ids, |b, ids| {
        b.iter(|| {
            let mut map: HashMap<ObjectId, u32> = HashMap::with_capacity(COUNT);
            for (i, id) in ids.iter().enumerate() {
                map.insert(black_box(*id), black_box(i as u32));
            }
            black_box(map.len())
        });
    });

    group.bench_with_input(BenchmarkId::new("oid_hasher", COUNT), &ids, |b, ids| {
        b.iter(|| {
            let mut map: OidHashMap<u32> = oid_hash_map_with_capacity(COUNT);
            for (i, id) in ids.iter().enumerate() {
                map.insert(black_box(*id), black_box(i as u32));
            }
            black_box(map.len())
        });
    });
    group.finish();
}

fn bench_insert_default_growth(c: &mut Criterion) {
    let ids = sample_ids(COUNT);
    let mut group = c.benchmark_group("oid_hashmap_insert_default_growth");
    group.sample_size(10);

    group.bench_with_input(BenchmarkId::new("siphash", COUNT), &ids, |b, ids| {
        b.iter(|| {
            let mut map: HashMap<ObjectId, u32> = HashMap::new();
            for (i, id) in ids.iter().enumerate() {
                map.insert(black_box(*id), black_box(i as u32));
            }
            black_box(map.len())
        });
    });

    group.bench_with_input(BenchmarkId::new("oid_hasher", COUNT), &ids, |b, ids| {
        b.iter(|| {
            let mut map: OidHashMap<u32> = oid_hash_map();
            for (i, id) in ids.iter().enumerate() {
                map.insert(black_box(*id), black_box(i as u32));
            }
            black_box(map.len())
        });
    });
    group.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let ids = sample_ids(COUNT);
    let mut siphash: HashMap<ObjectId, u32> = HashMap::new();
    let mut oid_map: OidHashMap<u32> = oid_hash_map();
    for (i, id) in ids.iter().enumerate() {
        siphash.insert(*id, i as u32);
        oid_map.insert(*id, i as u32);
    }
    let probes: Vec<ObjectId> = (0..10_000).map(|i| ids[i * (COUNT / 10_000)]).collect();

    let mut group = c.benchmark_group("oid_hashmap_lookup");
    group.bench_with_input(BenchmarkId::new("siphash", COUNT), &probes, |b, probes| {
        b.iter(|| {
            let mut sum = 0u64;
            for id in probes {
                if let Some(v) = siphash.get(black_box(id)) {
                    sum += u64::from(*v);
                }
            }
            black_box(sum)
        });
    });
    group.bench_with_input(
        BenchmarkId::new("oid_hasher", COUNT),
        &probes,
        |b, probes| {
            b.iter(|| {
                let mut sum = 0u64;
                for id in probes {
                    if let Some(v) = oid_map.get(black_box(id)) {
                        sum += u64::from(*v);
                    }
                }
                black_box(sum)
            });
        },
    );
    group.finish();
}

fn bench_contains(c: &mut Criterion) {
    let ids = sample_ids(COUNT);
    let mut siphash: HashSet<ObjectId> = HashSet::new();
    let mut oid_set: OidHashSet = oid_hash_set();
    for id in &ids {
        siphash.insert(*id);
        oid_set.insert(*id);
    }
    let probes: Vec<ObjectId> = (0..10_000).map(|i| ids[i * (COUNT / 10_000)]).collect();
    let missing = ObjectId::from_hex("ffffffffffffffffffffffffffffffffffffffff").expect("hex");

    let mut group = c.benchmark_group("oid_hashmap_contains");
    group.bench_with_input(BenchmarkId::new("siphash", COUNT), &probes, |b, probes| {
        b.iter(|| {
            let mut hits = 0usize;
            for id in probes {
                if siphash.contains(&black_box(*id)) {
                    hits += 1;
                }
            }
            if siphash.contains(&missing) {
                hits += 1;
            }
            black_box(hits)
        });
    });
    group.bench_with_input(
        BenchmarkId::new("oid_hasher", COUNT),
        &probes,
        |b, probes| {
            b.iter(|| {
                let mut hits = 0usize;
                for id in probes {
                    if oid_set.contains(&black_box(*id)) {
                        hits += 1;
                    }
                }
                if oid_set.contains(&missing) {
                    hits += 1;
                }
                black_box(hits)
            });
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    bench_insert_presized,
    bench_insert_default_growth,
    bench_lookup,
    bench_contains
);
criterion_main!(benches);

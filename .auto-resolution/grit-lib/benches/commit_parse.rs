//! Deterministic commit parser throughput (100k valid commits, no ODB fixture).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::objects::{parse_commit, CommitRef};

fn bench_bodies() -> &'static [Vec<u8>] {
    static BODIES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    BODIES.get_or_init(|| {
        let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let ident = "Bench <b@example.com> 1700000000 +0000";
        let mut bodies = Vec::with_capacity(100_000);
        let mut parent = empty.to_owned();
        for i in 0..100_000 {
            let body = format!(
                "tree {empty}\nparent {parent}\nauthor {ident}\ncommitter {ident}\nchange-id bench-{i:06}\n\nm{i}\n"
            );
            parent = format!("{:040x}", i + 1);
            bodies.push(body.into_bytes());
        }
        bodies
    })
}

fn bench_commit_parse(c: &mut Criterion) {
    let bodies = bench_bodies();
    let mut group = c.benchmark_group("commit_parse_100k");
    group.throughput(Throughput::Elements(bodies.len() as u64));
    group.bench_function("parse_commit", |b| {
        b.iter(|| {
            for raw in bodies {
                black_box(parse_commit(black_box(raw)).expect("parse"));
            }
        });
    });
    group.bench_function("commit_ref_parse", |b| {
        b.iter(|| {
            for raw in bodies {
                black_box(CommitRef::parse(black_box(raw)).expect("parse"));
            }
        });
    });
    group.finish();
}

criterion_group!(commit_parse, bench_commit_parse);
criterion_main!(commit_parse);

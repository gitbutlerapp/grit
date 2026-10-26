//! No-op local fetch with 3,000 packed branches and 2,000 packed tags.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "../tests/support/fetch_many_refs.rs"]
mod fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode};

fn bench(c: &mut Criterion) {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, client) = fixture::fixture(tmp.path(), 3000, 2000, false);
    let local = client.join(".git");
    let mut group = c.benchmark_group("fetch_local_many_refs");
    group.sample_size(20);
    for (name, tags) in [
        ("following", TagMode::Following),
        ("none", TagMode::None),
        ("all", TagMode::All),
    ] {
        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags,
            remote_name: Some("origin".into()),
            ..Default::default()
        };
        group.bench_function(name, |b| {
            b.iter(|| {
                let outcome =
                    fetch_local(black_box(&local), black_box(&remote), black_box(&opts)).unwrap();
                assert_eq!(outcome.transfer.objects, 0);
                black_box(outcome)
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

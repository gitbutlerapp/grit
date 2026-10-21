//! Criterion benchmarks for reftable ref listing and batched updates.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{update_refs, RefTransactionItem};
use grit_lib::refs::list_refs;
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

const REF_COUNT: usize = 100_000;
const PREFIX_REF_COUNT: usize = 1_000;
const TXN_CREATE_COUNT: usize = 10_000;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

struct ReftableBenchFixture {
    _dir: TempDir,
    git_dir: PathBuf,
    base_commit: ObjectId,
}

impl ReftableBenchFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main".to_owned()),
        )
        .expect("init reftable");
        let git_dir = dir.path().to_path_buf();
        let base_commit = ObjectId::from_hex(EMPTY_TREE).expect("tree oid");
        let mut batch = Vec::with_capacity(REF_COUNT);
        for i in 0..REF_COUNT {
            batch.push(RefTransactionItem {
                name: format!("refs/heads/b{i:05}"),
                new_oid: Some(base_commit),
                expected_old: None,
            });
        }
        update_refs(&git_dir, &batch).expect("seed refs");
        let mut stack =
            grit_lib::refs::reftable::ReftableStack::open(&git_dir).expect("open stack");
        stack.compact().expect("compact to single table");
        Self {
            _dir: dir,
            git_dir,
            base_commit,
        }
    }
}

fn bench_for_each_ref_100k(c: &mut Criterion) {
    let fx = ReftableBenchFixture::new();
    c.bench_function("refs_reftable/for_each_ref_100k", |b| {
        b.iter(|| {
            let listed = list_refs(&fx.git_dir, "refs/").expect("list");
            black_box(listed.len())
        });
    });
}

fn bench_prefix_seek(c: &mut Criterion) {
    let fx = ReftableBenchFixture::new();
    c.bench_function("refs_reftable/prefix_seek", |b| {
        b.iter(|| {
            let listed = list_refs(&fx.git_dir, "refs/heads/b05").expect("list prefix");
            assert_eq!(listed.len(), PREFIX_REF_COUNT);
            black_box(listed.len())
        });
    });
}

fn bench_transaction_create_10k(c: &mut Criterion) {
    let fx = ReftableBenchFixture::new();
    c.bench_function("refs_reftable/transaction_create_10k", |b| {
        b.iter_batched(
            || {
                let work = tempfile::tempdir().expect("workdir");
                std::fs::create_dir_all(work.path()).expect("mkdir");
                Repository::init(
                    work.path(),
                    &InitOptions::new()
                        .with_bare(true)
                        .with_initial_branch("main".to_owned()),
                )
                .expect("init");
                work
            },
            |work| {
                let git_dir = work.path();
                let mut batch = Vec::with_capacity(TXN_CREATE_COUNT);
                for i in 0..TXN_CREATE_COUNT {
                    batch.push(RefTransactionItem {
                        name: format!("refs/heads/t{i:05}"),
                        new_oid: Some(fx.base_commit),
                        expected_old: None,
                    });
                }
                update_refs(git_dir, &batch).expect("txn");
                black_box(batch.len())
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    refs_reftable,
    bench_for_each_ref_100k,
    bench_prefix_seek,
    bench_transaction_create_10k
);
criterion_main!(refs_reftable);

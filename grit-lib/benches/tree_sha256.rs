//! Criterion benchmark for [`grit_lib::tree_sha256`].

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::{TreeSha256, TreeSha256Options};

fn git(dir: &Path, args: &[&str]) -> String {
    grit_test_support::git(dir, args)
}

fn bench_tree_sha256_small(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..512 {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("payload {i}\n")).expect("write");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "many files"]);
    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
    let head: ObjectId = git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");
    let opts = TreeSha256Options::default();
    c.bench_function("tree_sha256_512_files", |b| {
        b.iter(|| {
            black_box(repo.tree_sha256(head, &opts).expect("tree_sha256").digest);
        });
    });
}

criterion_group!(benches, bench_tree_sha256_small);
criterion_main!(benches);

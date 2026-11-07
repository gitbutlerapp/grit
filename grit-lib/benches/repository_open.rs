//! Cost of `Repository::open` for the files layout, the files layout with read layers, and a
//! custom primary object store.

#![allow(clippy::expect_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::HashAlgo;
use grit_lib::repo::{InitOptions, Repository};

fn repository_open(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    Repository::init(&root, &InitOptions::default()).expect("init");
    let git_dir = root.join(".git");
    let files = RepositoryOptions::empty().with_work_tree(&root);
    let layered = files.clone().with_odb_builder(
        OdbBuilder::new().push_read_source(Arc::new(MemoryStore::new(HashAlgo::Sha1))),
    );
    let custom = files
        .clone()
        .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)));

    let mut group = c.benchmark_group("repository_open");
    for (name, options) in [
        ("files", &files),
        ("files_with_read_source", &layered),
        ("custom_store", &custom),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(Repository::open(&git_dir, options).expect("open")));
        });
    }
    group.bench_function("discover", |b| {
        b.iter(|| {
            black_box(
                Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("discover"),
            )
        });
    });
    group.finish();
}

criterion_group!(benches, repository_open);
criterion_main!(benches);

//! Criterion: structured commit-tree diff (blob load + line hunks) at smoke scale.
//!
//! Run manually when touching diff hot paths:
//! `cargo bench -p grit-lib --bench commit_range_diff`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::objects::FileMode;
use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::{DiffEntry, DiffOptions, TreeDiffOptions};
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

const FILE_COUNT: usize = 128;

struct CommitRangeFixture {
    root: TempDir,
    entries: Vec<DiffEntry>,
}

static FIXTURE: OnceLock<CommitRangeFixture> = OnceLock::new();

fn fixture() -> &'static CommitRangeFixture {
    FIXTURE.get_or_init(|| {
        let root = TempDir::new().expect("tempdir");
        let (repo, _) = Repository::init(root.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();

        let mut old_entries = Vec::with_capacity(FILE_COUNT);
        let mut new_entries = Vec::with_capacity(FILE_COUNT);
        for i in 0..FILE_COUNT {
            let old_blob = odb
                .write(ObjectKind::Blob, format!("old payload {i}\n").as_bytes())
                .expect("old blob");
            let new_blob = odb
                .write(ObjectKind::Blob, format!("new payload {i}\n").as_bytes())
                .expect("new blob");
            let name = format!("src/file_{i:04}.rs");
            old_entries.push(TreeEntry::new(
                name.as_bytes().to_vec(),
                FileMode::REGULAR,
                old_blob,
            ));
            new_entries.push(TreeEntry::new(
                name.into_bytes(),
                FileMode::REGULAR,
                new_blob,
            ));
        }
        let old_tree = odb
            .write(ObjectKind::Tree, &serialize_tree(&old_entries))
            .expect("old tree");
        let new_tree = odb
            .write(ObjectKind::Tree, &serialize_tree(&new_entries))
            .expect("new tree");
        let raw = repo
            .diff()
            .tree_to_tree(
                Some(&old_tree),
                Some(&new_tree),
                &TreeDiffOptions::default(),
            )
            .expect("diff trees")
            .entries;
        assert_eq!(raw.len(), FILE_COUNT);

        CommitRangeFixture { root, entries: raw }
    })
}

fn bench_commit_range_structured(c: &mut Criterion) {
    let fx = fixture();
    let opts = DiffOptions::default()
        .with_context_lines(3)
        .with_word_level_inline(false);
    let mut group = c.benchmark_group("commit_range_structured_diff");
    group.throughput(Throughput::Elements(FILE_COUNT as u64));
    for threads in [1usize, 0] {
        let label = if threads == 1 { "t1" } else { "auto" };
        group.bench_with_input(BenchmarkId::new("line_level", label), &threads, |b, &t| {
            let repo = Repository::discover(Some(fx.root.path()), &RepositoryOptions::empty())
                .expect("open")
                .with_thread_policy(ThreadPolicy::default().with_threads_override(Some(t)));
            b.iter(|| {
                black_box(
                    repo.diff()
                        .structured(fx.entries.clone(), &opts)
                        .expect("structured diff"),
                );
            });
        });
    }
    group.finish();
}

criterion_group!(
    name = commit_range_diff;
    config = Criterion::default().sample_size(20);
    targets = bench_commit_range_structured
);
criterion_main!(commit_range_diff);

//! Criterion: structured commit-tree diff (blob load + line hunks) at smoke scale.
//!
//! Run manually when touching diff hot paths:
//! `cargo bench -p grit-lib --bench commit_range_diff`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{diff_trees, tree_diff_structured_parallel, DiffEntry, DiffOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::parallel::ThreadPolicy;
use tempfile::TempDir;

const FILE_COUNT: usize = 128;

struct CommitRangeFixture {
    _root: TempDir,
    odb: Odb,
    entries: Vec<DiffEntry>,
    config: ConfigSet,
}

static FIXTURE: OnceLock<CommitRangeFixture> = OnceLock::new();

fn fixture() -> &'static CommitRangeFixture {
    FIXTURE.get_or_init(|| {
        let root = TempDir::new().expect("tempdir");
        let git_dir = root.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        let odb = Odb::new(&git_dir.join("objects"));
        let config = ConfigSet::new();

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
            old_entries.push(TreeEntry {
                mode: 0o100644,
                name: name.as_bytes().to_vec(),
                oid: old_blob,
            });
            new_entries.push(TreeEntry {
                mode: 0o100644,
                name: name.into_bytes(),
                oid: new_blob,
            });
        }
        let old_tree = odb
            .write(ObjectKind::Tree, &serialize_tree(&old_entries))
            .expect("old tree");
        let new_tree = odb
            .write(ObjectKind::Tree, &serialize_tree(&new_entries))
            .expect("new tree");
        let raw = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "").expect("diff trees");
        assert_eq!(raw.len(), FILE_COUNT);

        CommitRangeFixture {
            _root: root,
            odb,
            entries: raw,
            config,
        }
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
            let policy = ThreadPolicy::with_threads_override(Some(t));
            b.iter(|| {
                black_box(
                    tree_diff_structured_parallel(
                        &fx.odb,
                        fx.entries.clone(),
                        &fx.config,
                        &policy,
                        opts,
                    )
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

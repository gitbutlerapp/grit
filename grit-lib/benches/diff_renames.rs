//! Rename detection throughput at 2000×2000 candidate matrix.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::{detect_renames_with_policy, zero_oid, DiffEntry, DiffStatus};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectKind;
use grit_lib::parallel::ThreadPolicy;
use std::path::Path;
use tempfile::TempDir;

fn bench_config() -> ConfigSet {
    let text = "[diff]\n\trenameLimit = 5000\n[index]\n\tthreads = 4\n";
    let file = ConfigFile::parse(Path::new("bench"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

struct RenameBenchFixture {
    _root: TempDir,
    odb: Odb,
    entries: Vec<DiffEntry>,
    config: ConfigSet,
}

static FIXTURE: OnceLock<RenameBenchFixture> = OnceLock::new();

fn fixture() -> &'static RenameBenchFixture {
    FIXTURE.get_or_init(|| {
        let root = TempDir::new().expect("tempdir");
        let git_dir = root.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        let odb = Odb::new(&git_dir.join("objects"));

        const N: usize = 2000;
        let mut deleted = Vec::with_capacity(N);
        let mut added = Vec::with_capacity(N);
        for i in 0..N {
            let old_data = format!("deleted payload {i}\n").into_bytes();
            let new_data = format!("added payload {i}\n").into_bytes();
            let old_oid = odb.write(ObjectKind::Blob, &old_data).expect("write old");
            let new_oid = odb.write(ObjectKind::Blob, &new_data).expect("write new");
            deleted.push((format!("old/{i}.txt"), old_oid));
            added.push((format!("new/{i}.txt"), new_oid));
        }

        let mut entries: Vec<DiffEntry> = Vec::with_capacity(N * 2);
        for (path, oid) in deleted {
            entries.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path),
                new_path: None,
                old_mode: "100644".to_owned(),
                new_mode: "000000".to_owned(),
                old_oid: oid,
                new_oid: zero_oid(),
                score: None,
            });
        }
        for (path, oid) in added {
            entries.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path),
                old_mode: "000000".to_owned(),
                new_mode: "100644".to_owned(),
                old_oid: zero_oid(),
                new_oid: oid,
                score: None,
            });
        }

        let config = bench_config();

        RenameBenchFixture {
            _root: root,
            odb,
            entries,
            config,
        }
    })
}

fn bench_diff_renames(c: &mut Criterion) {
    let fx = fixture();
    let mut group = c.benchmark_group("diff_renames");
    group.throughput(Throughput::Elements(
        u64::try_from(fx.entries.len()).expect("count"),
    ));
    for threads in [1usize, 4] {
        let policy = ThreadPolicy::with_threads_override(Some(threads))
            .min_items_override(Some(0))
            .min_total_bytes_override(Some(0));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("threads={threads}")),
            &threads,
            |b, _| {
                b.iter(|| {
                    black_box(detect_renames_with_policy(
                        &fx.odb,
                        None,
                        fx.entries.clone(),
                        50,
                        &fx.config,
                        &policy,
                    ));
                });
            },
        );
    }
    group.finish();
}

fn adoc_like_body(seed: usize, lines: usize) -> Vec<u8> {
    let mut out = format!("= Documentation chapter {seed}\n\n").into_bytes();
    for line in 0..lines {
        out.extend_from_slice(format!("Paragraph {line}: lorem ipsum dolor sit amet.\n").as_bytes());
    }
    out
}

fn bench_diff_renames_adoc_similar(c: &mut Criterion) {
    let root = TempDir::new().expect("tempdir");
    let git_dir = root.path().join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    let odb = Odb::new(&git_dir.join("objects"));

    const N: usize = 64;
    const LINES: usize = 400;
    let mut entries: Vec<DiffEntry> = Vec::with_capacity(N * 2);
    for i in 0..N {
        let old_data = adoc_like_body(i, LINES);
        let mut new_data = adoc_like_body(i, LINES);
        new_data.extend_from_slice(b"\n== Added section\n\nNew content.\n");
        let old_oid = odb.write(ObjectKind::Blob, &old_data).expect("write old");
        let new_oid = odb.write(ObjectKind::Blob, &new_data).expect("write new");
        entries.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(format!("old/doc-{i}.txt")),
            new_path: None,
            old_mode: "100644".to_owned(),
            new_mode: "000000".to_owned(),
            old_oid,
            new_oid: zero_oid(),
            score: None,
        });
        entries.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(format!("new/doc-{i}.adoc")),
            old_mode: "000000".to_owned(),
            new_mode: "100644".to_owned(),
            old_oid: zero_oid(),
            new_oid,
            score: None,
        });
    }

    let config = bench_config();
    let policy = ThreadPolicy::with_threads_override(Some(4))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0));

    let mut group = c.benchmark_group("diff_renames_adoc_similar");
    group.throughput(Throughput::Elements(u64::try_from(N * 2).expect("count")));
    group.bench_function("detect_64_similar_doc_pairs", |b| {
        b.iter(|| {
            black_box(detect_renames_with_policy(
                &odb,
                None,
                entries.clone(),
                50,
                &config,
                &policy,
            ));
        });
    });
    group.finish();
}

criterion_group!(diff_renames, bench_diff_renames, bench_diff_renames_adoc_similar);
criterion_main!(diff_renames);

//! Rename detection throughput for wide matrices and similar documents.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::{zero_oid, DiffEntry, DiffStatus, RenameDetection, TreeDiffOptions};
use grit_lib::objects::{FileMode, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

const BENCH_CONFIG: &str = "[diff]\n\trenameLimit = 5000\n[index]\n\tthreads = 4\n";

/// A repository under `root` whose local config ends with `extra_config`.
fn bench_repo(root: &TempDir, extra_config: &str) -> Repository {
    Repository::init(root.path(), &InitOptions::default()).expect("init");
    let config_path = root.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config_path).expect("read config");
    text.push_str(extra_config);
    std::fs::write(&config_path, text).expect("write config");
    Repository::discover(Some(root.path()), &RepositoryOptions::empty()).expect("open")
}

fn rename_options(policy: ThreadPolicy) -> TreeDiffOptions {
    TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_threads(Some(policy))
}

struct RenameBenchFixture {
    _root: TempDir,
    repo: Repository,
    entries: Vec<DiffEntry>,
}

static FIXTURE: OnceLock<RenameBenchFixture> = OnceLock::new();

fn fixture() -> &'static RenameBenchFixture {
    FIXTURE.get_or_init(|| {
        let root = TempDir::new().expect("tempdir");
        let repo = bench_repo(&root, BENCH_CONFIG);
        let odb = repo.odb();

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
            entries.push(DiffEntry::new(
                DiffStatus::Deleted,
                Some(path),
                None,
                FileMode::REGULAR,
                FileMode::ABSENT,
                oid,
                zero_oid(),
            ));
        }
        for (path, oid) in added {
            entries.push(DiffEntry::new(
                DiffStatus::Added,
                None,
                Some(path),
                FileMode::ABSENT,
                FileMode::REGULAR,
                zero_oid(),
                oid,
            ));
        }

        RenameBenchFixture {
            _root: root,
            repo,
            entries,
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
                    black_box(
                        fx.repo
                            .diff()
                            .detect_renames(fx.entries.clone(), None, &rename_options(policy))
                            .expect("detect renames")
                            .entries,
                    );
                });
            },
        );
    }
    group.finish();
}

fn adoc_like_body(seed: usize, lines: usize, unique: bool) -> Vec<u8> {
    let mut out = format!("= Documentation chapter {seed}\n\n").into_bytes();
    for line in 0..lines {
        let paragraph = if unique {
            format!("Chapter {seed:04} paragraph {line:03}: documentation content.\n")
        } else {
            format!("Paragraph {line}: lorem ipsum dolor sit amet.\n")
        };
        out.extend_from_slice(paragraph.as_bytes());
    }
    out
}

fn adoc_fixture(count: usize, lines: usize, unique: bool) -> RenameBenchFixture {
    let extra_config = BENCH_CONFIG;
    let root = TempDir::new().expect("tempdir");
    let repo = bench_repo(&root, extra_config);
    let odb = repo.odb();

    let mut entries: Vec<DiffEntry> = Vec::with_capacity(count * 2);
    for i in 0..count {
        let old_data = adoc_like_body(i, lines, unique);
        let mut new_data = adoc_like_body(i, lines, unique);
        if unique {
            // A one-line edit after moving uniquely named documents forces the
            // full inexact matrix, rather than exact or basename matching.
            new_data[0] = b'#';
        } else {
            new_data.extend_from_slice(b"\n== Added section\n\nNew content.\n");
        }
        let old_oid = odb.write(ObjectKind::Blob, &old_data).expect("write old");
        let new_oid = odb.write(ObjectKind::Blob, &new_data).expect("write new");
        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("old/doc-{i}.txt")),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            old_oid,
            zero_oid(),
        ));
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("new/doc-{i}.adoc")),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            new_oid,
        ));
    }

    RenameBenchFixture {
        _root: root,
        repo,
        entries,
    }
}

fn bench_diff_renames_adoc_similar(c: &mut Criterion) {
    let policy = ThreadPolicy::with_threads_override(Some(4))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0));
    let mut group = c.benchmark_group("diff_renames_adoc_similar");
    group.sample_size(10);
    for (name, count, lines, unique) in [
        ("common_64", 64, 400, false),
        ("unique_256", 256, 100, true),
    ] {
        let fx = adoc_fixture(count, lines, unique);
        group.throughput(Throughput::Elements(
            u64::try_from(count * 2).expect("count"),
        ));
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(
                    fx.repo
                        .diff()
                        .detect_renames(fx.entries.clone(), None, &rename_options(policy))
                        .expect("detect renames")
                        .entries,
                );
            });
        });
    }
    group.finish();
}

/// Duplicate `readme.txt` basenames with directory votes seeded by exact renames.
fn basename_duplicate_fixture(count: usize, rename_limit: i64) -> RenameBenchFixture {
    let extra_config = &format!("{BENCH_CONFIG}[diff]\n\trenameLimit = {rename_limit}\n");
    let root = TempDir::new().expect("tempdir");
    let repo = bench_repo(&root, extra_config);
    let odb = repo.odb();

    let mut entries: Vec<DiffEntry> = Vec::with_capacity(count * 4);
    for i in 0..count {
        let anchor = format!("anchor {i}\n").into_bytes();
        let anchor_oid = odb.write(ObjectKind::Blob, &anchor).expect("anchor");

        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("old/{i}/anchor.txt")),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            anchor_oid,
            zero_oid(),
        ));
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("new/{i}/anchor.txt")),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            anchor_oid,
        ));

        let old_readme = format!("readme body {i} line one\nline two\n").into_bytes();
        let mut new_readme = old_readme.clone();
        new_readme.extend_from_slice(b"line three edited\n");
        let old_oid = odb
            .write(ObjectKind::Blob, &old_readme)
            .expect("old readme");
        let new_oid = odb
            .write(ObjectKind::Blob, &new_readme)
            .expect("new readme");

        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("old/{i}/readme.txt")),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            old_oid,
            zero_oid(),
        ));
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("new/{i}/readme.txt")),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            new_oid,
        ));
    }

    RenameBenchFixture {
        _root: root,
        repo,
        entries,
    }
}

/// Issue #2436: repeated but disjoint basenames (`f*` delete vs `g*` add).
fn basename_disjoint_duplicate_fixture(count: usize) -> RenameBenchFixture {
    let extra_config = BENCH_CONFIG;
    let root = TempDir::new().expect("tempdir");
    let repo = bench_repo(&root, extra_config);
    let odb = repo.odb();
    let mut entries: Vec<DiffEntry> = Vec::with_capacity(count * 2);
    for i in 0..count {
        let old_oid = odb
            .write(ObjectKind::Blob, format!("old payload {i}\n").as_bytes())
            .expect("write old");
        let new_oid = odb
            .write(ObjectKind::Blob, format!("new payload {i}\n").as_bytes())
            .expect("write new");
        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("a/d{}/f{}.txt", i / 50, i % 50)),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            old_oid,
            zero_oid(),
        ));
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("b/d{}/g{}.txt", i / 50, i % 50)),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            new_oid,
        ));
    }
    RenameBenchFixture {
        _root: root,
        repo,
        entries,
    }
}

fn bench_diff_renames_basename_disjoint_duplicates(c: &mut Criterion) {
    let policy = ThreadPolicy::with_threads_override(Some(1))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0));
    let mut group = c.benchmark_group("diff_renames_basename_disjoint_duplicates");
    group.sample_size(10);
    for count in [512_usize, 2048] {
        let fx = basename_disjoint_duplicate_fixture(count);
        group.throughput(Throughput::Elements(
            u64::try_from(count * 2).expect("count"),
        ));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(
                    fx.repo
                        .diff()
                        .detect_renames(fx.entries.clone(), None, &rename_options(policy))
                        .expect("detect renames")
                        .entries,
                );
            });
        });
    }
    group.finish();
}

fn bench_diff_renames_basename_duplicates(c: &mut Criterion) {
    let policy = ThreadPolicy::with_threads_override(Some(1))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0));
    let mut group = c.benchmark_group("diff_renames_basename_duplicates");
    group.sample_size(10);
    for (name, count, limit) in [
        ("limited_128", 128, 500_i64),
        ("unlimited_64", 64, 5000_i64),
    ] {
        let fx = basename_duplicate_fixture(count, limit);
        group.throughput(Throughput::Elements(
            u64::try_from(count * 4).expect("count"),
        ));
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(
                    fx.repo
                        .diff()
                        .detect_renames(fx.entries.clone(), None, &rename_options(policy))
                        .expect("detect renames")
                        .entries,
                );
            });
        });
    }
    group.finish();
}

criterion_group!(
    diff_renames,
    bench_diff_renames,
    bench_diff_renames_adoc_similar,
    bench_diff_renames_basename_disjoint_duplicates,
    bench_diff_renames_basename_duplicates
);
criterion_main!(diff_renames);

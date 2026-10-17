//! Files-backend ref listing and batch updates on 100k-ref fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{update_refs, RefTransactionItem};
use grit_lib::refs::list_refs;

fn refs_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_REFS_CACHE")
        .or_else(|_| std::env::var("GRIT_BENCH_SCRATCH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-refs-cache"))
}

fn fixture_repo(kind: &str) -> PathBuf {
    refs_cache_root().join(format!("refs-100k-{kind}"))
}

fn fixture_ready(path: &Path) -> bool {
    path.join("packed-refs").exists() || path.join("refs").is_dir()
}

#[derive(Clone)]
struct RefsFilesFixture {
    git_dir: PathBuf,
}

impl RefsFilesFixture {
    fn open(kind: &str) -> Option<Self> {
        static PACKED: OnceLock<Option<RefsFilesFixture>> = OnceLock::new();
        static LOOSE: OnceLock<Option<RefsFilesFixture>> = OnceLock::new();
        let cell = if kind == "packed" { &PACKED } else { &LOOSE };
        cell.get_or_init(|| {
            let git_dir = fixture_repo(kind);
            if !fixture_ready(&git_dir) {
                eprintln!(
                    "SKIP refs_files: fixture missing at {} (run grit-bench refs first)",
                    git_dir.display()
                );
                return None;
            }
            Some(RefsFilesFixture { git_dir })
        })
        .clone()
    }
}

fn bench_list_all(c: &mut Criterion, kind: &str) {
    let Some(fx) = RefsFilesFixture::open(kind) else {
        return;
    };
    let mut group = c.benchmark_group(format!("refs_files/for_each_ref_100k_{kind}"));
    group.bench_function("list_refs_all", |b| {
        b.iter(|| {
            let rows = list_refs(black_box(&fx.git_dir), "refs/").expect("list");
            black_box(rows.len())
        });
    });
    group.bench_function("list_refs_prefix_b05", |b| {
        b.iter(|| {
            let rows = list_refs(black_box(&fx.git_dir), "refs/heads/b05").expect("list");
            black_box(rows.len())
        });
    });
    group.finish();
}

fn bench_transaction_create(c: &mut Criterion) {
    let base = fixture_repo("packed");
    let create_stdin = base.join("create-100k.stdin");
    if !create_stdin.is_file() {
        eprintln!(
            "SKIP refs_files/transaction_create_10k: missing {}",
            create_stdin.display()
        );
        return;
    }
    let template = std::fs::read_to_string(&create_stdin).expect("stdin");
    let lines: Vec<_> = template.lines().take(10_000).collect();
    let oid = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
    let mut group = c.benchmark_group("refs_files/transaction_create_10k");
    group.bench_function("update_refs_create", |b| {
        b.iter_batched(
            || tempfile::TempDir::new().expect("tempdir"),
            |tmp| {
                let git_dir = tmp.path().join(".git");
                std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
                std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
                let items: Vec<RefTransactionItem> = lines
                    .iter()
                    .filter_map(|line| {
                        let mut parts = line.split_whitespace();
                        let cmd = parts.next()?;
                        if cmd != "create" {
                            return None;
                        }
                        let name = parts.next()?.to_owned();
                        Some(RefTransactionItem {
                            name,
                            new_oid: Some(oid),
                            expected_old: None,
                        })
                    })
                    .collect();
                update_refs(&git_dir, &items).expect("batch");
                black_box(items.len())
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_packed(c: &mut Criterion) {
    bench_list_all(c, "packed");
}

fn bench_loose(c: &mut Criterion) {
    bench_list_all(c, "loose");
}

criterion_group!(
    refs_files,
    bench_packed,
    bench_loose,
    bench_transaction_create
);
criterion_main!(refs_files);

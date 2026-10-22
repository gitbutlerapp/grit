//! Version sort on large ref fixtures vs `git for-each-ref`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::refs::listing::{
    query_listings_on_store, RefNamespace, RefQuery, RefQueryOptions, RefSort, SortKey,
};
use grit_lib::refs::store::open_ref_store;

fn refs_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_REFS_CACHE")
        .or_else(|_| std::env::var("GRIT_BENCH_SCRATCH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-refs-cache"))
}

fn fixture_git_dir(kind: &str) -> PathBuf {
    refs_cache_root().join(format!("refs-100k-{kind}"))
}

fn fixture_ready(path: &Path) -> bool {
    path.join("packed-refs").exists() || path.join("refs").is_dir()
}

fn bench_version_sort(c: &mut Criterion) {
    static GIT_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let git_dir = GIT_DIR
        .get_or_init(|| {
            let path = fixture_git_dir("packed");
            if fixture_ready(&path) {
                Some(path)
            } else {
                eprintln!(
                    "SKIP ref_query_sort_version_100k: missing fixture at {} (run grit-bench refs)",
                    path.display()
                );
                None
            }
        })
        .clone();
    let Some(git_dir) = git_dir else {
        return;
    };

    let mut group = c.benchmark_group("ref_query_sort_version_100k");
    group.bench_function("grit_version_sort", |b| {
        b.iter(|| {
            let store = open_ref_store(&git_dir).expect("open store");
            let query = RefQuery {
                namespaces: vec![RefNamespace::Heads],
                patterns: vec![],
                sort: RefSort {
                    key: SortKey::Version,
                    reverse: false,
                },
                limit: None,
            };
            let rows =
                query_listings_on_store(store.as_ref(), &query, &RefQueryOptions::default(), None)
                    .expect("query");
            black_box(rows.len())
        });
    });

    group.bench_function("git_for_each_ref_version", |b| {
        b.iter(|| {
            let start = Instant::now();
            let out = Command::new("git")
                .args([
                    "--git-dir",
                    git_dir.to_str().expect("utf8"),
                    "for-each-ref",
                    "--sort=version:refname",
                    "--format=%(refname)",
                    "refs/heads/",
                ])
                .output()
                .expect("git");
            assert!(out.status.success());
            black_box(out.stdout.len());
            black_box(start.elapsed())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_version_sort);
criterion_main!(benches);

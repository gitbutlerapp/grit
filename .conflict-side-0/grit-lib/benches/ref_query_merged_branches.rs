//! Branch `--merged` ref query vs `git for-each-ref --merged` on many-refs fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::listing::{query_refs, RefNamespace, RefQuery, RefQueryOptions};
use grit_lib::repo::Repository;

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

fn bench_merged_branches(c: &mut Criterion) {
    static GIT_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let git_dir = GIT_DIR
        .get_or_init(|| {
            let path = fixture_git_dir("packed");
            if fixture_ready(&path) {
                Some(path)
            } else {
                eprintln!(
                    "SKIP ref_query_merged_branches: missing fixture at {} (run grit-bench refs)",
                    path.display()
                );
                None
            }
        })
        .clone();
    let Some(git_dir) = git_dir else {
        return;
    };
    let merged = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().expect("utf8"),
            "rev-parse",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .and_then(|h| ObjectId::from_hex(&h).ok());
    let Some(merged) = merged else {
        eprintln!("SKIP ref_query_merged_branches: no HEAD");
        return;
    };
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open bare");
    let merged_hex = merged.to_string();

    let mut group = c.benchmark_group("ref_query_merged_branches");
    group.bench_function("grit_merged_branches", |b| {
        b.iter(|| {
            let query = RefQuery {
                namespaces: vec![RefNamespace::Heads],
                merged: vec![merged],
                ..RefQuery::default()
            };
            let n = query_refs(&repo, &query, &RefQueryOptions::default())
                .expect("query")
                .len();
            std::hint::black_box(n)
        });
    });
    group.bench_function("git_for_each_ref_merged", |b| {
        b.iter(|| {
            let start = Instant::now();
            let out = Command::new("git")
                .args([
                    "--git-dir",
                    git_dir.to_str().expect("utf8"),
                    "for-each-ref",
                    "refs/heads/",
                    &format!("--merged={merged_hex}"),
                    "--format=%(refname)",
                ])
                .output()
                .expect("git");
            assert!(out.status.success());
            std::hint::black_box(out.stdout.len());
            std::hint::black_box(start.elapsed())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_merged_branches);
criterion_main!(benches);

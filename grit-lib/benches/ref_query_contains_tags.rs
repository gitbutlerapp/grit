//! Tag `--contains` ref query vs `git for-each-ref --contains` on a deep-history fixture.

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

fn bench_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-cache"))
}

fn git_git_worktree() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dest = bench_cache_root().join("git-git");
        if dest.join("HEAD").exists() || dest.join(".git").join("HEAD").exists() {
            Some(dest)
        } else {
            eprintln!(
                "SKIP ref_query_contains_tags: missing git-git at {} (run grit-bench fixtures prepare git-git)",
                dest.display()
            );
            None
        }
    })
    .clone()
}

fn mid_history_commit(worktree: &Path) -> Option<ObjectId> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "HEAD~5000"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hex = String::from_utf8(out.stdout).ok()?.trim().to_string();
    ObjectId::from_hex(&hex).ok()
}

fn bench_contains_tags(c: &mut Criterion) {
    let Some(worktree) = git_git_worktree() else {
        return;
    };
    let Some(contains) = mid_history_commit(&worktree) else {
        eprintln!("SKIP ref_query_contains_tags: history too shallow");
        return;
    };
    let repo =
        Repository::discover(Some(&worktree), &RepositoryOptions::empty()).expect("open git-git");
    let contains_hex = contains.to_string();

    let mut group = c.benchmark_group("ref_query_contains_tags");
    group.bench_function("grit_contains_tags", |b| {
        b.iter(|| {
            let query = RefQuery {
                namespaces: vec![RefNamespace::Tags],
                contains: vec![contains],
                ..RefQuery::default()
            };
            let n = query_refs(&repo, &query, &RefQueryOptions::default())
                .expect("query")
                .len();
            std::hint::black_box(n)
        });
    });
    group.bench_function("git_for_each_ref_contains", |b| {
        b.iter(|| {
            let start = Instant::now();
            let out = Command::new("git")
                .current_dir(&worktree)
                .args([
                    "for-each-ref",
                    "refs/tags/",
                    &format!("--contains={contains_hex}"),
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

/// Unique packed branch tips keep a bounded query from accidentally benchmarking
/// only shared-tip deduplication. Build objects with grit and pack them with system Git.
fn bench_bounded_branches(c: &mut Criterion) {
    use criterion::{BatchSize, BenchmarkId};
    use grit_lib::objects::ObjectKind;
    use grit_lib::repo::InitOptions;

    let dir = tempfile::tempdir().expect("fixture");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
    let mut commits = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let parent = commits
            .last()
            .map(|p| format!("parent {p}\n"))
            .unwrap_or_default();
        let bytes = format!("tree {tree}\n{parent}author T <t@example.com> 1700000000 +0000\ncommitter T <t@example.com> 1700000000 +0000\n\ncommit {i}\n");
        commits.push(
            repo.odb()
                .write(ObjectKind::Commit, bytes.as_bytes())
                .expect("commit"),
        );
    }
    let target = commits[9899];
    let mut refs = String::from("# pack-refs with: sorted\n");
    for (i, oid) in commits.iter().rev().enumerate() {
        refs.push_str(&format!("{oid} refs/heads/dogfood/{i:05}\n"));
    }
    std::fs::write(repo.git_dir().join("packed-refs"), refs).expect("packed refs");
    assert!(Command::new("git")
        .current_dir(dir.path())
        .args(["repack", "-adq"])
        .status()
        .expect("pack")
        .success());
    let mut group = c.benchmark_group("ref_query_contains_10k_unique_branches");
    group.sample_size(10);
    for graph in [false, true] {
        if graph {
            assert!(Command::new("git")
                .current_dir(dir.path())
                .args(["commit-graph", "write", "--reachable"])
                .status()
                .expect("graph")
                .success());
        }
        for limit in [Some(1), Some(100), Some(1000), None] {
            let label = limit
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unlimited".into());
            group.bench_with_input(
                BenchmarkId::new(if graph { "graph" } else { "no_graph" }, label),
                &limit,
                |b, &limit| {
                    b.iter_batched(
                        || {
                            Repository::discover(Some(dir.path()), &RepositoryOptions::empty())
                                .expect("open")
                        },
                        |repo| {
                            let query = RefQuery {
                                contains: vec![target],
                                limit,
                                ..RefQuery::default()
                            };
                            std::hint::black_box(
                                query_refs(&repo, &query, &RefQueryOptions::default())
                                    .expect("query"),
                            )
                        },
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, bench_contains_tags, bench_bounded_branches);
criterion_main!(benches);

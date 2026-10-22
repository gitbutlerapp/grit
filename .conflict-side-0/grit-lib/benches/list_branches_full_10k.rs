//! Full branch inventory on large ref fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::branch_tracking::{list_branches, BranchDetail, BranchListQuery, BranchScope};
use grit_lib::refs::listing::{RefQuery, SortKey};
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

fn bench_list_branches_full(c: &mut Criterion) {
    static WT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let git_dir = WT
        .get_or_init(|| {
            let path = fixture_git_dir("packed");
            if fixture_ready(&path) {
                Some(path)
            } else {
                eprintln!(
                    "SKIP list_branches_full_10k: missing fixture at {} (run grit-bench refs)",
                    path.display()
                );
                None
            }
        })
        .clone();
    let Some(git_dir) = git_dir else {
        return;
    };

    c.bench_function("branch_inventory/list_branches_full_10k", |b| {
        b.iter(|| {
            let repo = Repository::open(
                &git_dir,
                &RepositoryOptions::empty().with_work_tree(&git_dir),
            )
            .expect("open");
            let query = BranchListQuery {
                scope: BranchScope::Local,
                refs: RefQuery {
                    limit: Some(10_000),
                    sort: grit_lib::refs::listing::RefSort {
                        key: SortKey::Name,
                        reverse: false,
                    },
                    ..RefQuery::default()
                },
            };
            let rows = list_branches(black_box(&repo), black_box(&query), BranchDetail::Full)
                .expect("list");
            black_box(rows.len())
        });
    });
}

criterion_group!(benches, bench_list_branches_full);
criterion_main!(benches);

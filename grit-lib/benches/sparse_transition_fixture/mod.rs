use std::path::Path;

use criterion::{BenchmarkId, Criterion};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;

pub fn bench_sparse_transitions(c: &mut Criterion) {
    let mut group = c.benchmark_group("checkout/sparse_300_visible");
    group.sample_size(10);
    for (paths, unique, changed) in [
        (120_000, false, 0),
        (240_000, false, 0),
        (120_000, true, 5000),
    ] {
        let dir = tempfile::TempDir::new().expect("tempdir");
        grit_test_support::sparse_scale::create(
            Path::new("git"),
            dir.path(),
            paths,
            unique,
            changed,
        )
        .expect("fixture");
        let mut env = Environment::empty();
        env.cwd = dir.path().to_path_buf();
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::with_environment(env))
                .expect("repo");
        let endpoints: Vec<ObjectId> = ["sparse-base", "sparse-edited"]
            .into_iter()
            .map(|name| {
                grit_test_support::git(dir.path(), &["rev-parse", name])
                    .trim()
                    .parse()
                    .expect("oid")
            })
            .collect();
        let label = format!("{paths}_unique_{unique}_changed_{changed}");
        group.bench_function(BenchmarkId::new("reset", &label), |b| {
            b.iter(|| {
                for target in &endpoints {
                    reset_to_commit(
                        &repo,
                        &ResetOptions::new(
                            *target,
                            ResetMode::Hard,
                            "Mono Dev <mono@example.com> 1700000020 +0000"
                                .parse()
                                .expect("signature"),
                            "reset",
                        ),
                        &mut NullProgress,
                        &NeverCancel,
                    )
                    .expect("reset");
                }
            })
        });
        group.bench_function(BenchmarkId::new("switch", &label), |b| {
            b.iter(|| {
                for target in &endpoints {
                    switch_repository(
                        &repo,
                        &SwitchOptions {
                            mode: SwitchMode::Detached {
                                commit: *target,
                                reflog_to_label: target.to_string(),
                            },
                            dry_run: false,
                            hooks: grit_lib::PostOperationHooks::Skip,
                        },
                        &mut NullProgress,
                        &NeverCancel,
                    )
                    .expect("switch");
                }
            })
        });
    }
    group.finish();
}

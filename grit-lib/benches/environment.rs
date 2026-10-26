//! Cost of cloning explicit environment snapshots during repository/config setup.

use std::ffi::OsString;
use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use grit_lib::config::environment::Environment;

fn environment_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("environment_clone");
    for count in [0, 64, 256, 1024] {
        let env = Environment::from_vars(
            (0..count).map(|i| {
                (
                    OsString::from(format!("BUILD_VARIABLE_{i}")),
                    OsString::from("a representative inherited variable value"),
                )
            }),
            PathBuf::from("/workspace/project"),
        );
        group.bench_with_input(BenchmarkId::from_parameter(count), &env, |b, env| {
            b.iter(|| black_box(black_box(env).clone()));
        });
    }
    group.finish();
}

criterion_group!(benches, environment_clone);
criterion_main!(benches);

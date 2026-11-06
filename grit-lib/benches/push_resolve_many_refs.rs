//! Resolve ~1000 shorthand push refspecs against thousands of local heads.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::environment::Environment;
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::remote::push_plan::{resolve_push, PushPlanContext, PushRequest};
use grit_lib::repo::{InitOptions, Repository};

fn fixture() -> (tempfile::TempDir, Repository, ConfigSet, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).unwrap();
    let git_dir = repo.git_dir();
    std::fs::write(
        git_dir.join("config"),
        r#"[remote "origin"]
	url = /tmp/o.git
"#,
    )
    .unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let oid = ObjectId::from_hex(&"a".repeat(40)).unwrap();
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    for i in 0..6000 {
        std::fs::write(
            git_dir.join(format!("refs/heads/b{i}")),
            format!("{}\n", oid.to_hex()),
        )
        .unwrap();
    }
    let config = ConfigSet::load(
        &Environment::empty(),
        Some(git_dir),
        &LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
    )
    .unwrap();
    let refspecs: Vec<String> = (0..1000).map(|i| format!("b{i}")).collect();
    (tmp, repo, config, refspecs)
}

fn bench(c: &mut Criterion) {
    let (_tmp, repo, config, refspecs) = fixture();
    let ctx = PushPlanContext::default();
    c.bench_function("push_resolve_1000_shorthands_6000_heads", |b| {
        b.iter(|| {
            let request = PushRequest {
                remote: Some("origin".into()),
                refspecs: refspecs.clone(),
                ..Default::default()
            };
            let resolved =
                resolve_push(black_box(&repo), black_box(&request), black_box(&ctx)).unwrap();
            assert_eq!(resolved.specs.len(), 1000);
            black_box(resolved)
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);

//! Warm change-id prefix resolution on a packed, headerless git.git-scale clone.
//!
//! Fixture: bare clone at `GRIT_GIT_GIT_BARE` or `/tmp/grit-bench-odb-cache/git.git`
//! (115 MiB-class packed upstream git.git). Measures repeated
//! [`grit_lib::change_id::find_commits_by_change_id_prefix`] after the negative probe
//! cache is primed — the regression that reread every pack byte per lookup.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::change_id::{find_commits_by_change_id_prefix, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

struct ChangeIdResolveFixture {
    repo: Repository,
    prefix: String,
}

fn git_git_bare() -> PathBuf {
    std::env::var("GRIT_GIT_GIT_BARE")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("GRIT_BENCH_ODB_CACHE").map(PathBuf::from))
        .map(|p| p.join("git.git"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-odb-cache/git.git"))
}

fn run_git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn load_fixture() -> Option<ChangeIdResolveFixture> {
    let bare = git_git_bare();
    if !bare.is_dir() {
        eprintln!(
            "SKIP change_id_resolve bench: bare repo not found at {}",
            bare.display()
        );
        return None;
    }
    if !bare.join("objects/pack").is_dir() {
        eprintln!(
            "SKIP change_id_resolve bench: no pack directory at {}",
            bare.display()
        );
        return None;
    }
    let _ = run_git(&bare, &["gc", "--prune=now"]);
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).ok()?;
    let out = Command::new("git")
        .current_dir(&bare)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head_hex = std::str::from_utf8(&out.stdout).ok()?.trim();
    let oid = grit_lib::objects::ObjectId::from_hex(head_hex.as_bytes()).ok()?;
    let derived = ChangeId::derived_from_commit_id(&oid);
    let prefix = derived.short(12).to_string();
    find_commits_by_change_id_prefix(&repo, &prefix).ok()?;
    Some(ChangeIdResolveFixture { repo, prefix })
}

fn fixture() -> &'static ChangeIdResolveFixture {
    static FIXTURE: OnceLock<Option<ChangeIdResolveFixture>> = OnceLock::new();
    FIXTURE
        .get_or_init(load_fixture)
        .as_ref()
        .expect("change_id_resolve bench fixture unavailable")
}

fn bench_change_id_resolve(c: &mut Criterion) {
    if !git_git_bare().is_dir() {
        eprintln!("SKIP change_id_resolve benchmarks (no git.git bare clone)");
        return;
    }
    let fx = fixture();
    let mut group = c.benchmark_group("change_id_resolve_git_sized");
    group.bench_function("find_by_prefix_warm", |b| {
        b.iter(|| {
            black_box(
                find_commits_by_change_id_prefix(&fx.repo, &fx.prefix)
                    .expect("resolve change-id prefix"),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, bench_change_id_resolve);
criterion_main!(benches);

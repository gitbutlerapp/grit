//! Warm change-id prefix resolution on a packed, headerless git.git-scale clone.
//!
//! Fixture: bare clone at `GRIT_GIT_GIT_BARE` or `/tmp/grit-bench-odb-cache/git.git`
//! (115 MiB-class packed upstream git.git). Measures repeated
//! [`grit_lib::change_id::find_commits_by_change_id_prefix`] after the negative probe
//! cache is primed — the regression that reread every pack byte per lookup.
//!
//! `find_by_prefix_warm_memory_store` runs the same lookup with the clone's commits and
//! tags copied into a [`MemoryStore`] primary. A custom store has no on-disk probe to say
//! that no commit carries a header, so every lookup scans each reachable commit's header
//! through the store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::change_id::{find_commits_by_change_id_prefix, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, WritableObjectStore};
use grit_lib::objects::odb::WriteOptions;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::repo::Repository;

struct ChangeIdResolveFixture {
    repo: Repository,
    memory_repo: Repository,
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

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Open `bare` with its commits and tags (all a change-id lookup reads) copied into a
/// memory store; refs stay in the bare repository's files ref store.
fn open_memory_store_repo(bare: &Path, files: &Repository) -> Option<Repository> {
    let listing = git_stdout(
        bare,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    )?;
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    for line in listing.lines() {
        let (hex, kind) = line.split_once(' ')?;
        if kind != "commit" && kind != "tag" {
            continue;
        }
        let oid = ObjectId::from_hex(hex).ok()?;
        let obj = files.odb().read(&oid).ok()?;
        store
            .write(obj.kind, &obj.data, WriteOptions::default())
            .ok()?;
    }
    Repository::open(bare, &RepositoryOptions::empty().with_object_store(store)).ok()
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
    let head_hex = git_stdout(&bare, &["rev-parse", "HEAD"])?;
    let oid = ObjectId::from_hex(head_hex.trim()).ok()?;
    let derived = ChangeId::derived_from_commit_id(&oid);
    let prefix = derived.short(12).to_string();
    find_commits_by_change_id_prefix(&repo, &prefix).ok()?;
    let memory_repo = open_memory_store_repo(&bare, &repo)?;
    Some(ChangeIdResolveFixture {
        repo,
        memory_repo,
        prefix,
    })
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
    group.bench_function("find_by_prefix_warm_memory_store", |b| {
        b.iter(|| {
            black_box(
                find_commits_by_change_id_prefix(&fx.memory_repo, &fx.prefix)
                    .expect("resolve change-id prefix through a memory store"),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, bench_change_id_resolve);
criterion_main!(benches);

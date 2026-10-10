//! Pack reachability bitmap open and first commit lookup (git.git-scale when available).
//!
//! Fixture: bare clone at `GRIT_GIT_GIT_BARE` or `/tmp/git.git` with `git repack -adb`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::bitmap_walk::ReachabilityQuery;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pack::read_local_pack_indexes;
use grit_lib::pack_bitmap::{BitmapIndex, PackBitmapWriteOptions, PackBitmapWriter};
use grit_lib::pack_object_select::{enumerate_pack_objects, PackEnumerateOptions};
use grit_lib::repo::Repository;
use grit_lib::rev_list::MissingAction;

struct PackBitmapFixture {
    repo: Repository,
    sample_commit: ObjectId,
    have_commit: ObjectId,
    odb: Odb,
}

fn git_git_bare() -> PathBuf {
    std::env::var("GRIT_GIT_GIT_BARE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/git.git"))
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

fn ensure_pack_bitmap(bare: &Path) -> bool {
    let pack_dir = bare.join("objects/pack");
    let has_bitmap = std::fs::read_dir(&pack_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"));
    if has_bitmap {
        return true;
    }
    run_git(bare, &["repack", "-adb"])
}

fn load_fixture() -> Option<PackBitmapFixture> {
    let bare = git_git_bare();
    if !bare.is_dir() {
        eprintln!(
            "SKIP pack_bitmap bench: bare repo not found at {}",
            bare.display()
        );
        return None;
    }
    if !ensure_pack_bitmap(&bare) {
        eprintln!(
            "SKIP pack_bitmap bench: repack -adb failed at {}",
            bare.display()
        );
        return None;
    }
    let repo = Repository::open(&bare, None).ok()?;
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
    let sample_commit = ObjectId::from_hex(std::str::from_utf8(&out.stdout).ok()?.trim()).ok()?;
    let have_out = Command::new("git")
        .current_dir(&bare)
        .args(["rev-parse", "HEAD~100"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !have_out.status.success() {
        return None;
    }
    let have_commit =
        ObjectId::from_hex(std::str::from_utf8(&have_out.stdout).ok()?.trim()).ok()?;
    let odb = Odb::new(&bare.join("objects")).with_config_git_dir(bare.to_path_buf());
    Some(PackBitmapFixture {
        repo,
        sample_commit,
        have_commit,
        odb,
    })
}

fn fixture() -> &'static PackBitmapFixture {
    static FIXTURE: OnceLock<Option<PackBitmapFixture>> = OnceLock::new();
    FIXTURE
        .get_or_init(load_fixture)
        .as_ref()
        .expect("pack_bitmap bench fixture unavailable")
}

fn bench_pack_bitmap(c: &mut Criterion) {
    if git_git_bare().is_dir() {
        let _ = fixture();
    } else {
        eprintln!("SKIP pack_bitmap benchmarks (no git.git bare clone)");
        return;
    }
    let fx = fixture();

    let bare = git_git_bare();
    let mut group = c.benchmark_group("pack_bitmap_git_sized");
    group.bench_function("open", |b| {
        b.iter(|| {
            let repo = Repository::open(&bare, None).expect("open repo");
            black_box(
                BitmapIndex::open(&repo)
                    .expect("open")
                    .expect("bitmap present"),
            );
        });
    });
    group.bench_function("first_commit_lookup", |b| {
        b.iter(|| {
            let repo = Repository::open(&bare, None).expect("open repo");
            let index = BitmapIndex::open(&repo)
                .expect("open")
                .expect("bitmap present");
            black_box(index.commit_bitmap(&fx.sample_commit));
        });
    });
    group.bench_function("reachability_count_all", |b| {
        b.iter(|| {
            let repo = Repository::open(&bare, None).expect("open repo");
            let index = BitmapIndex::open(&repo)
                .expect("open")
                .expect("bitmap present");
            let query = ReachabilityQuery {
                wants: &[fx.sample_commit],
                haves: &[],
                filter: None,
            };
            let set = index
                .reachability(&repo, query, MissingAction::Error)
                .expect("reachability");
            black_box(set.count());
        });
    });
    group.bench_function("write_from_git_bitmap", |b| {
        b.iter(|| {
            let repo = Repository::open(&bare, None).expect("open repo");
            let indexes = read_local_pack_indexes(bare.join("objects").as_path()).expect("idx");
            let idx = indexes
                .into_iter()
                .max_by_key(|i| i.len())
                .expect("pack")
                .idx_path;
            let _ = std::fs::remove_file({
                let stem = idx.file_stem().and_then(|s| s.to_str()).unwrap_or("pack");
                bare.join("objects/pack").join(format!("{stem}.bitmap"))
            });
            black_box(
                PackBitmapWriter::write(
                    &repo,
                    &idx,
                    &PackBitmapWriteOptions::default(),
                    std::time::SystemTime::UNIX_EPOCH,
                )
                .expect("write"),
            );
        });
    });
    group.bench_function("pack_enumerate_walk", |b| {
        let empty = HashSet::new();
        b.iter(|| {
            black_box(
                enumerate_pack_objects(
                    &fx.odb,
                    &[fx.sample_commit],
                    &[fx.have_commit],
                    &PackEnumerateOptions {
                        filter: None,
                        shallow_grafts: &empty,
                        use_bitmaps: false,
                        exclude_objects: None,
                    },
                )
                .expect("enumerate"),
            );
        });
    });
    group.bench_function("pack_enumerate_bitmap", |b| {
        let empty = HashSet::new();
        b.iter(|| {
            black_box(
                enumerate_pack_objects(
                    &fx.odb,
                    &[fx.sample_commit],
                    &[fx.have_commit],
                    &PackEnumerateOptions {
                        filter: None,
                        shallow_grafts: &empty,
                        use_bitmaps: true,
                        exclude_objects: None,
                    },
                )
                .expect("enumerate"),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, bench_pack_bitmap);
criterion_main!(benches);

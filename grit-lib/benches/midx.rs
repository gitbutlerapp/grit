//! Criterion benchmarks for multi-pack-index lookup vs per-pack index scans.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::Path;
use std::process::Command;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::midx::{midx_lookup_pack_and_offset_opt, try_read_object_via_midx};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_pack_index_cached};
use std::sync::Arc;

struct MidxBenchFixture {
    objects: std::path::PathBuf,
    hit: ObjectId,
    miss: ObjectId,
    pack_indexes: Vec<Arc<grit_lib::pack::PackIndex>>,
}

impl MidxBenchFixture {
    fn build(pack_count: usize) -> Option<Self> {
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        let git_ok = Command::new("git")
            .current_dir(dir)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            return None;
        }
        std::fs::write(dir.join("seed.txt"), b"x").ok()?;
        run_git(dir, &["add", "seed.txt"]);
        run_git(dir, &["commit", "-q", "-m", "seed"]);
        for i in 0..pack_count {
            std::fs::write(dir.join(format!("p{i}.txt")), format!("data-{i}")).ok()?;
            run_git(dir, &["add", &format!("p{i}.txt")]);
            run_git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
            pack_layer(dir, i, i + 1 == pack_count);
        }
        run_git(dir, &["multi-pack-index", "write"]);
        let objects = dir.join(".git/objects");
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let hit = ObjectId::from_hex(std::str::from_utf8(&out.stdout).ok()?.trim()).ok()?;
        let miss = ObjectId::from_hex("00000000000000000000000000000000000000f1").ok()?;
        let pack_dir = objects.join("pack");
        let mut pack_indexes: Vec<Arc<grit_lib::pack::PackIndex>> = std::fs::read_dir(&pack_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "idx"))
            .map(|p| read_pack_index_cached(&p).expect("idx"))
            .collect();
        pack_indexes.sort_by_key(|idx| idx.idx_path.clone());
        std::mem::forget(tmp);
        Some(Self {
            objects,
            hit,
            miss,
            pack_indexes,
        })
    }
}

fn pack_layer(dir: &Path, layer: usize, all_objects: bool) {
    let rev = if all_objects {
        Command::new("git")
            .current_dir(dir)
            .args(["rev-list", "--objects", "--all"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-list")
    } else {
        Command::new("git")
            .current_dir(dir)
            .args(["rev-list", "--objects", "-1", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-list")
    };
    assert!(rev.status.success());
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", &format!(".git/objects/pack/layer-{layer}")])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("pack-objects");
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&rev.stdout)
        .expect("write rev-list");
    let out = child.wait_with_output().expect("wait pack-objects");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {:?}", args);
}

fn lookup_all_packs(idxs: &[Arc<grit_lib::pack::PackIndex>], oid: &ObjectId) -> bool {
    let mut found = false;
    for idx in idxs {
        if idx.find_offset(oid).is_some() {
            found = true;
        }
    }
    found
}

fn lookup_all_packs_miss(idxs: &[Arc<grit_lib::pack::PackIndex>], oid: &ObjectId) -> bool {
    idxs.iter().all(|idx| idx.find_offset(oid).is_none())
}

fn bench_midx_lookup(c: &mut Criterion) {
    let Some(fx) = MidxBenchFixture::build(10) else {
        eprintln!("SKIP midx bench: git fixture unavailable");
        return;
    };
    clear_pack_cache();
    let _ = try_read_object_via_midx(&fx.objects, &fx.hit).expect("warm midx cache");
    let _ = midx_lookup_pack_and_offset_opt(&fx.objects, &fx.hit).expect("warm lookup api");

    let mut group = c.benchmark_group("midx_lookup");
    group.bench_function("hit", |b| {
        b.iter(|| black_box(midx_lookup_pack_and_offset_opt(&fx.objects, &fx.hit).unwrap()));
    });
    group.bench_function("miss", |b| {
        b.iter(|| black_box(midx_lookup_pack_and_offset_opt(&fx.objects, &fx.miss).unwrap()));
    });
    group.bench_function("all_pack_idx_hit", |b| {
        b.iter(|| black_box(lookup_all_packs(&fx.pack_indexes, &fx.hit)));
    });
    group.bench_function("all_pack_idx_miss", |b| {
        b.iter(|| black_box(lookup_all_packs_miss(&fx.pack_indexes, &fx.miss)));
    });
    group.finish();
}

fn bench_midx_batch_reads(c: &mut Criterion) {
    let Some(fx) = MidxBenchFixture::build(10) else {
        eprintln!("SKIP midx batch bench: git fixture unavailable");
        return;
    };
    clear_pack_cache();
    let git_dir = fx.objects.parent().expect("git dir");
    let odb = Odb::new(&fx.objects).with_config_git_dir(git_dir.to_path_buf());
    let _ = odb.read(&fx.hit).expect("warm");

    let mut oids = vec![fx.hit];
    for i in 0..9 {
        oids.push(fx.hit);
        let _ = i;
    }
    let mut batch: Vec<ObjectId> = Vec::with_capacity(30_000);
    while batch.len() < 30_000 {
        batch.extend(oids.iter().copied());
    }
    batch.truncate(30_000);

    let mut group = c.benchmark_group("midx_batch_30k");
    group.sample_size(10);
    group.bench_function("grit_odb_read", |b| {
        b.iter(|| {
            for oid in &batch {
                black_box(odb.read(oid).expect("read"));
            }
        });
    });
    group.bench_function("grit_midx_read", |b| {
        b.iter(|| {
            for oid in &batch {
                black_box(
                    try_read_object_via_midx(&fx.objects, oid)
                        .expect("read")
                        .expect("obj"),
                );
            }
        });
    });
    group.bench_function("git_cat_file_batch", |b| {
        b.iter(|| {
            git_cat_file_batch(git_dir, &batch);
        });
    });
    group.finish();
}

fn git_cat_file_batch(git_dir: &Path, oids: &[ObjectId]) {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .env("GIT_DIR", git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("git cat-file");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for oid in oids {
            writeln!(stdin, "{}", oid.to_hex()).expect("write");
        }
    }
    let status = child.wait().expect("wait");
    assert!(status.success());
}

criterion_group!(midx, bench_midx_lookup, bench_midx_batch_reads);
criterion_main!(midx);

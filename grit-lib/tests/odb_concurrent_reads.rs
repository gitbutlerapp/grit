//! Concurrent [`Odb::read`] from worker threads (pack store context and byte identity).

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use grit_lib::error::Error;
use grit_lib::objects::midx::{write_multi_pack_index_with_options, WriteMultiPackIndexOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::{clear_pack_cache, read_pack_index};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::transfer::{build_pack, PackBuildOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::test_support::pack_store;
use tempfile::TempDir;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn odb_repository_and_pack_store_are_send_sync() {
    assert_send_sync::<Odb>();
    assert_send_sync::<grit_lib::objects::pack_store::PackStore>();
    assert_send_sync::<grit_lib::repo::Repository>();
}

fn install_pack(objects_dir: &Path, stem: &str, pack: &[u8]) -> Vec<ObjectId> {
    let scratch = tempfile::tempdir().expect("index-pack scratch");
    let pack_path = scratch.path().join(format!("{stem}.pack"));
    fs::write(&pack_path, pack).expect("write pack");
    let out = std::process::Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", &format!("{stem}.pack")])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn index-pack");
    assert!(
        out.status.success(),
        "index-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dest_dir = objects_dir.join("pack");
    fs::create_dir_all(&dest_dir).expect("pack dir");
    fs::copy(
        scratch.path().join(format!("{stem}.pack")),
        dest_dir.join(format!("{stem}.pack")),
    )
    .expect("copy pack");
    fs::copy(
        scratch.path().join(format!("{stem}.idx")),
        dest_dir.join(format!("{stem}.idx")),
    )
    .expect("copy idx");
    clear_pack_cache();
    let idx = read_pack_index(&dest_dir.join(format!("{stem}.idx"))).expect("read idx");
    idx.iter()
        .filter_map(|e| ObjectId::from_bytes(e.oid()).ok())
        .collect()
}

#[test]
fn odb_concurrent_reads() {
    clear_pack_cache();
    pack_store::reset();

    let root = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init repo");
    let primary_objects = repo.git_dir().join("objects");

    let mut packed_blob_oids = Vec::new();
    for batch in 0..4 {
        let mut batch_oids = Vec::new();
        for i in 0..16 {
            let n = batch * 16 + i;
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, format!("packed blob {n}\n").as_bytes())
                .expect("write blob for pack");
            batch_oids.push(oid);
        }
        let pack = build_pack(
            repo.odb(),
            &batch_oids,
            &[],
            &PackBuildOptions {
                delta: true,
                max_depth: 32,
                use_ofs_delta: true,
                ..PackBuildOptions::default()
            },
        )
        .expect("build pack");
        install_pack(&primary_objects, &format!("p{batch}"), &pack);
        packed_blob_oids.extend(batch_oids);
    }

    let loose_oid = repo
        .odb()
        .write(ObjectKind::Blob, b"loose-only concurrent read\n")
        .expect("loose write");

    let alt_root = TempDir::new().expect("alt tempdir");
    let alt_objects = alt_root.path().join("objects");
    fs::create_dir_all(&alt_objects).expect("alt objects");
    let alt_odb = Odb::new(&alt_objects);
    let alt_blob = alt_odb
        .write(ObjectKind::Blob, b"from-alternate-store\n")
        .expect("alt blob");
    let alt_pack =
        build_pack(&alt_odb, &[alt_blob], &[], &PackBuildOptions::default()).expect("alt pack");
    install_pack(&alt_objects, "alt", &alt_pack);

    fs::create_dir_all(primary_objects.join("info")).expect("info");
    fs::write(
        primary_objects.join("info/alternates"),
        format!("{}\n", alt_objects.display()),
    )
    .expect("alternates");

    let pack_dir = primary_objects.join("pack");
    write_multi_pack_index_with_options(&pack_dir, &WriteMultiPackIndexOptions::default())
        .expect("write midx");
    let probe = packed_blob_oids[0];
    grit_lib::objects::midx::try_read_object_via_midx(&primary_objects, &probe)
        .expect("midx read probe")
        .expect("midx must list probe oid");

    let odb = Odb::with_work_tree(&primary_objects, root.path())
        .with_config_git_dir(repo.git_dir().to_path_buf());

    let mut oids = packed_blob_oids;
    oids.push(loose_oid);
    oids.push(alt_blob);
    assert!(oids.len() >= 10, "need a mixed object sample");

    let serial: Vec<Vec<u8>> = oids
        .iter()
        .map(|oid| {
            odb.read(oid)
                .unwrap_or_else(|e| panic!("serial read {}: {e}", oid.to_hex()))
                .data
        })
        .collect();

    pack_store::reset();

    let stop = Arc::new(AtomicBool::new(false));
    let odb_inv = odb.clone();
    let stop_inv = Arc::clone(&stop);
    let invalidator = std::thread::spawn(move || {
        while !stop_inv.load(Ordering::Acquire) {
            odb_inv.invalidate_packs();
            std::thread::yield_now();
        }
    });

    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let b = Arc::clone(&barrier);
        let odb_t = odb.clone();
        let sample = oids.clone();
        let expect = serial.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            for (oid, want) in sample.iter().zip(expect.iter()) {
                let got = odb_t.read(oid).expect("parallel read").data;
                assert_eq!(&got, want, "byte mismatch for {}", oid.to_hex());
            }
        }));
    }
    for h in handles {
        h.join().expect("reader thread");
    }

    stop.store(true, Ordering::Release);
    invalidator.join().expect("invalidator");

    assert_eq!(
        pack_store::legacy_stores_created(),
        0,
        "Odb reads must not create legacy thread-local pack stores"
    );
    assert_eq!(
        pack_store::implicit_stores_created(),
        0,
        "Odb reads must not create implicit pack stores"
    );

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let odb_s = odb.clone();
            let oid = oids[0];
            scope.spawn(move || {
                odb_s.read(&oid).expect("scoped read");
            });
        }
    });

    assert_eq!(pack_store::legacy_stores_created(), 0);
    assert_eq!(pack_store::implicit_stores_created(), 0);
}

/// Regression: a thread holding `with_pack_read_context` must not return bytes after another
/// thread removed packs and called `Odb::invalidate_packs`.
#[test]
fn stale_read_after_invalidation() {
    clear_pack_cache();
    pack_store::reset();

    let root = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init repo");
    let objects = repo.git_dir().join("objects");
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, b"only-in-pack\n")
        .expect("write blob");
    let pack = build_pack(
        repo.odb(),
        &[oid],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    install_pack(&objects, "solo", &pack);
    let hex = oid.to_hex();
    let loose_path = objects.join(&hex[..2]).join(&hex[2..]);
    let _ = fs::remove_file(&loose_path);

    let odb = Odb::with_work_tree(&objects, root.path());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let odb_reader = odb.clone();
    let reader = std::thread::spawn(move || {
        odb_reader.with_pack_read_context(|| {
            let _ = odb_reader.read(&oid).expect("prime read inside context");
            ready_tx.send(()).expect("ready");
            done_rx.recv().expect("wait for invalidation");
            match odb_reader.read(&oid) {
                Err(Error::ObjectNotFound(_)) => false,
                Ok(_) => true,
                Err(other) => panic!("unexpected read error: {other}"),
            }
        })
    });

    ready_rx.recv().expect("reader warmed");
    let pack_dir = objects.join("pack");
    for entry in fs::read_dir(&pack_dir).expect("list pack dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "pack" || e == "idx") {
            fs::remove_file(path).expect("remove pack artifact");
        }
    }
    odb.invalidate_packs();
    done_tx.send(()).expect("release reader");
    let stale_read = reader.join().expect("reader join");
    assert!(
        !stale_read,
        "read after cross-thread invalidation must not return removed pack objects"
    );
    assert!(
        matches!(odb.read(&oid), Err(Error::ObjectNotFound(_))),
        "serial read after pack removal must miss"
    );
}

/// Two [`PackStore`] instances can share epoch 0; warming must not reuse the first store's TLS.
#[test]
fn pack_thread_local_warm_follows_active_store_on_one_thread() {
    clear_pack_cache();
    let tmp_a = TempDir::new().expect("tempdir a");
    let tmp_b = TempDir::new().expect("tempdir b");
    let objects_a = tmp_a.path().join("objects");
    let objects_b = tmp_b.path().join("objects");
    std::fs::create_dir_all(objects_a.join("pack")).expect("pack a");
    std::fs::create_dir_all(objects_b.join("pack")).expect("pack b");
    let odb_a = Odb::new(&objects_a);
    let odb_b = Odb::new(&objects_b);
    let oid_a = odb_a
        .write(ObjectKind::Blob, b"store-a\n")
        .expect("write a");
    let oid_b = odb_b
        .write(ObjectKind::Blob, b"store-b\n")
        .expect("write b");
    let pack_a = build_pack(
        &odb_a,
        &[oid_a],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack a");
    let pack_b = build_pack(
        &odb_b,
        &[oid_b],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack b");
    install_pack(&objects_a, "a", &pack_a);
    install_pack(&objects_b, "b", &pack_b);
    grit_lib::objects::pack::read_object_from_packs(&objects_a, &oid_a)
        .expect("prime legacy store a");
    let got_b = odb_b.read(&oid_b).expect("read via odb b store");
    assert_eq!(got_b.data, b"store-b\n");
}

#[cfg(target_os = "linux")]
fn proc_maps_regions_for(path: &Path) -> usize {
    let needle = path.to_string_lossy();
    std::fs::read_to_string("/proc/self/maps")
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle.as_ref()))
        .count()
}

/// Regression (#2495): repeated open/read/drop on one thread must not retain pack/idx mmaps.
#[test]
#[cfg(target_os = "linux")]
fn pack_thread_local_released_when_store_dropped_on_same_thread() {
    clear_pack_cache();
    let root = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init repo");
    let objects = repo.git_dir().join("objects");
    let mut blob_oids = Vec::new();
    for i in 0..64 {
        let mut payload = vec![0u8; 16 * 1024];
        for (j, b) in payload.iter_mut().enumerate() {
            *b = ((i * 997 + j) & 0xff) as u8;
        }
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, &payload)
            .expect("write blob");
        blob_oids.push(oid);
    }
    let pack = build_pack(
        repo.odb(),
        &blob_oids,
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    assert!(
        pack.len() > 8192,
        "pack must exceed mmap ownership threshold for this regression"
    );
    install_pack(&objects, "big", &pack);
    let probe = blob_oids[0];
    let pack_path = objects.join("pack/big.pack");
    let idx_path = objects.join("pack/big.idx");

    let mut baseline = None;
    for _ in 0..20 {
        let odb = Odb::with_work_tree(&objects, root.path());
        odb.read_info(&probe).expect("read in loop");
        let pack_regions = proc_maps_regions_for(&pack_path);
        let idx_regions = proc_maps_regions_for(&idx_path);
        if let Some((base_pack, base_idx)) = baseline {
            assert_eq!(
                pack_regions, base_pack,
                "pack mmap regions must not grow with repeated store drops"
            );
            assert_eq!(
                idx_regions, base_idx,
                "idx mmap regions must not grow with repeated store drops"
            );
        } else {
            baseline = Some((pack_regions, idx_regions));
        }
    }
}

/// Dropping one store on a thread must not release another live store's warm TLS.
#[test]
#[cfg(target_os = "linux")]
fn pack_thread_local_drop_one_store_leaves_sibling_warm() {
    clear_pack_cache();
    let tmp_a = TempDir::new().expect("tempdir a");
    let tmp_b = TempDir::new().expect("tempdir b");
    let objects_a = tmp_a.path().join("objects");
    let objects_b = tmp_b.path().join("objects");
    std::fs::create_dir_all(objects_a.join("pack")).expect("pack a");
    std::fs::create_dir_all(objects_b.join("pack")).expect("pack b");
    let odb_a = Odb::new(&objects_a);
    let odb_b = Odb::new(&objects_b);
    let payload = format!("{}\n", "y".repeat(9000));
    let oid_a = odb_a
        .write(ObjectKind::Blob, payload.as_bytes())
        .expect("write a");
    let oid_b = odb_b
        .write(ObjectKind::Blob, b"store-b sibling\n")
        .expect("write b");
    let pack_a = build_pack(
        &odb_a,
        &[oid_a],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack a");
    let pack_b = build_pack(
        &odb_b,
        &[oid_b],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack b");
    install_pack(&objects_a, "a", &pack_a);
    install_pack(&objects_b, "b", &pack_b);

    odb_b.read(&oid_b).expect("warm store b");
    {
        let odb_a_short = Odb::new(&objects_a);
        odb_a_short.read(&oid_a).expect("warm store a");
    }
    let got_b = odb_b.read(&oid_b).expect("store b still readable");
    assert_eq!(got_b.data, b"store-b sibling\n");

    let pack_b_path = objects_b.join("pack/b.pack");
    let idx_b_path = objects_b.join("pack/b.idx");
    let baseline_pack_b = proc_maps_regions_for(&pack_b_path);
    let baseline_idx_b = proc_maps_regions_for(&idx_b_path);
    for _ in 0..10 {
        let odb_a_short = Odb::new(&objects_a);
        odb_a_short.read(&oid_a).expect("read a");
    }
    assert_eq!(
        proc_maps_regions_for(&pack_b_path),
        baseline_pack_b,
        "sibling store b pack mapping must survive drops of store a"
    );
    assert_eq!(
        proc_maps_regions_for(&idx_b_path),
        baseline_idx_b,
        "sibling store b idx mapping must survive drops of store a"
    );
    odb_b
        .read(&oid_b)
        .expect("store b still warm after a drops");
}

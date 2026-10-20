//! Concurrent [`Odb::read`] from worker threads (pack store context and byte identity).

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use grit_lib::error::Error;
use grit_lib::objects::midx::{write_multi_pack_index_with_options, WriteMultiPackIndexOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::{clear_pack_cache, read_pack_index};
use grit_lib::objects::pack_store::test_hooks;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::remote::transfer::{build_pack, PackBuildOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;
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
    test_hooks::reset();

    let root = TempDir::new().expect("tempdir");
    let repo = Repository::init(
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

    test_hooks::reset();

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
        test_hooks::legacy_stores_created(),
        0,
        "Odb reads must not create legacy thread-local pack stores"
    );
    assert_eq!(
        test_hooks::implicit_stores_created(),
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

    assert_eq!(test_hooks::legacy_stores_created(), 0);
    assert_eq!(test_hooks::implicit_stores_created(), 0);
}

/// Regression: a thread holding `with_pack_read_context` must not return bytes after another
/// thread removed packs and called `Odb::invalidate_packs`.
#[test]
fn stale_read_after_invalidation() {
    clear_pack_cache();
    test_hooks::reset();

    let root = TempDir::new().expect("tempdir");
    let repo = Repository::init(
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

/// Regression: [`SINGLE_PACK_READER`] must not stay mutably borrowed across pack reads that
/// invalidate thread-local caches (reentrant `RefCell` panic on this branch).
#[test]
fn single_pack_reader_survives_invalidation_between_reads_on_one_thread() {
    clear_pack_cache();
    test_hooks::reset();

    let root = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init repo");
    let objects = repo.git_dir().join("objects");
    let mut oids = Vec::new();
    for i in 0..32 {
        oids.push(
            repo.odb()
                .write(
                    ObjectKind::Blob,
                    format!("single-pack blob {i}\n").as_bytes(),
                )
                .expect("write blob"),
        );
    }
    let pack = build_pack(
        repo.odb(),
        &oids,
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 32,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    install_pack(&objects, "only", &pack);
    for oid in &oids {
        let hex = oid.to_hex();
        let loose = objects.join(&hex[..2]).join(&hex[2..]);
        let _ = fs::remove_file(loose);
    }

    let odb = Odb::with_work_tree(&objects, root.path());
    odb.with_pack_read_context(|| {
        for oid in &oids {
            let _ = odb.read(oid).expect("read before invalidation");
            odb.invalidate_packs();
            let _ = odb.read(oid).expect("read after same-thread invalidation");
        }
    });
}

#[test]
fn single_pack_reader_concurrent_reads_with_invalidation() {
    clear_pack_cache();
    test_hooks::reset();

    let root = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init repo");
    let objects = repo.git_dir().join("objects");
    let mut oids = Vec::new();
    for i in 0..24 {
        oids.push(
            repo.odb()
                .write(
                    ObjectKind::Blob,
                    format!("concurrent single-pack {i}\n").as_bytes(),
                )
                .expect("write blob"),
        );
    }
    let pack = build_pack(
        repo.odb(),
        &oids,
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 32,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("build pack");
    install_pack(&objects, "only", &pack);
    for oid in &oids {
        let hex = oid.to_hex();
        let loose = objects.join(&hex[..2]).join(&hex[2..]);
        let _ = fs::remove_file(loose);
    }

    let odb = Odb::with_work_tree(&objects, root.path());
    let stop = Arc::new(AtomicBool::new(false));
    let odb_inv = odb.clone();
    let stop_inv = Arc::clone(&stop);
    let invalidator = std::thread::spawn(move || {
        while !stop_inv.load(Ordering::Acquire) {
            odb_inv.invalidate_packs();
            std::thread::yield_now();
        }
    });

    let threads = 6;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let b = Arc::clone(&barrier);
        let odb_t = odb.clone();
        let sample = oids.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            odb_t.with_pack_read_context(|| {
                for oid in &sample {
                    let _ = odb_t.read(oid).expect("concurrent single-pack read");
                }
            });
        }));
    }
    for h in handles {
        h.join().expect("reader thread");
    }
    stop.store(true, Ordering::Release);
    invalidator.join().expect("invalidator");
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

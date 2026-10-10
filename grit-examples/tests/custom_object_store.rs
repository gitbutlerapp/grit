//! [`PackfileKvStore`] conformance and Git interoperability after export.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;

use flate2::Compression;
use grit_examples::packfile_kv::PackfileKvStore;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{LooseStore, ObjectStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};

fn leak_temp_kv(algo: HashAlgo) -> PackfileKvStore {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let path = dir.path().join("objects.kv");
    PackfileKvStore::create(path, algo).expect("create kv store")
}

fn seed_kv(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> PackfileKvStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let store = leak_temp_kv(algo);
        for (kind, data) in specs {
            WritableObjectStore::write(&store, *kind, data, WriteOptions::default())
                .expect("seed write");
        }
        PackfileKvStore::open(store.path()).expect("reopen kv store")
    }
}

fn make_kv(algo: HashAlgo) -> impl Fn() -> PackfileKvStore {
    move || leak_temp_kv(algo)
}

#[test]
fn packfile_kv_sha1_read_suite() {
    run_read_suite(seed_kv(HashAlgo::Sha1));
}

#[test]
fn packfile_kv_sha1_write_suite() {
    run_write_suite(make_kv(HashAlgo::Sha1));
}

#[test]
fn packfile_kv_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_kv(HashAlgo::Sha256));
}

#[test]
fn packfile_kv_sha256_write_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_write_suite(make_kv(HashAlgo::Sha256));
}

#[test]
fn packfile_kv_reopen_append_reopen() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    let path = dir.path().join("objects.kv");
    let oid_a = {
        let store = PackfileKvStore::create(&path, HashAlgo::Sha1)?;
        WritableObjectStore::write(&store, ObjectKind::Blob, b"a", WriteOptions::default())?
    };
    let oid_b = {
        let store = PackfileKvStore::open(&path)?;
        WritableObjectStore::write(&store, ObjectKind::Blob, b"b", WriteOptions::default())?
    };
    let store = PackfileKvStore::open(&path)?;
    assert_eq!(store.read(&oid_a)?.expect("first blob").data, b"a");
    assert_eq!(store.read(&oid_b)?.expect("second blob").data, b"b");
    Ok(())
}

#[test]
fn packfile_kv_recovers_from_torn_length_prefix() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    let path = dir.path().join("objects.kv");
    let oid_a = {
        let store = PackfileKvStore::create(&path, HashAlgo::Sha1)?;
        WritableObjectStore::write(&store, ObjectKind::Blob, b"first", WriteOptions::default())?
    };
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(grit_lib::error::Error::Io)?;
        file.write_all(&[0x00])
            .map_err(grit_lib::error::Error::Io)?;
    }
    let oid_b = {
        let store = PackfileKvStore::open(&path)?;
        assert_eq!(
            store
                .read(&oid_a)?
                .expect("first blob after torn tail")
                .data,
            b"first"
        );
        WritableObjectStore::write(&store, ObjectKind::Blob, b"second", WriteOptions::default())?
    };
    let store = PackfileKvStore::open(&path)?;
    assert_eq!(store.read(&oid_b)?.expect("second blob").data, b"second");
    Ok(())
}

#[test]
fn custom_object_store_example_exports_git_clean_repo() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    let grit_log = grit_examples::packfile_kv::run_custom_object_store_demo(dir.path())?;
    assert_eq!(grit_log.len(), 1);

    let fsck = Command::new("git")
        .current_dir(dir.path())
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let git_log = Command::new("git")
        .current_dir(dir.path())
        .args(["log", "--format=%H"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git log");
    assert!(git_log.status.success());
    let git_lines: Vec<String> = String::from_utf8_lossy(&git_log.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(git_lines, grit_log);

    let objects_dir = dir.path().join(".git/objects");
    let loose = LooseStore::new(objects_dir, HashAlgo::Sha1, Compression::default());
    assert!(loose
        .read(&grit_log[0].parse().expect("commit oid"))?
        .is_some());
    Ok(())
}

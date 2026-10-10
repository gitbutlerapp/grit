//! [`SqliteOdbStore`] conformance and Git interoperability after export.

use std::process::Command;

use flate2::Compression;
use grit_examples::sqlite_odb::SqliteOdbStore;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{LooseStore, ObjectStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};

fn leak_temp_sqlite(algo: HashAlgo) -> SqliteOdbStore {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let path = dir.path().join("objects.sqlite");
    SqliteOdbStore::create(path, algo).expect("create sqlite store")
}

fn seed_sqlite(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> SqliteOdbStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let store = leak_temp_sqlite(algo);
        for (kind, data) in specs {
            WritableObjectStore::write(&store, *kind, data, WriteOptions::default())
                .expect("seed write");
        }
        SqliteOdbStore::open(store.path()).expect("reopen sqlite store")
    }
}

fn make_sqlite(algo: HashAlgo) -> impl Fn() -> SqliteOdbStore {
    move || leak_temp_sqlite(algo)
}

#[test]
fn sqlite_odb_sha1_read_suite() {
    run_read_suite(seed_sqlite(HashAlgo::Sha1));
}

#[test]
fn sqlite_odb_sha1_write_suite() {
    run_write_suite(make_sqlite(HashAlgo::Sha1));
}

#[test]
fn sqlite_odb_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_sqlite(HashAlgo::Sha256));
}

#[test]
fn sqlite_odb_sha256_write_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_write_suite(make_sqlite(HashAlgo::Sha256));
}

#[test]
fn sqlite_odb_reopen_append_reopen() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    let path = dir.path().join("objects.sqlite");
    let oid_a = {
        let store = SqliteOdbStore::create(&path, HashAlgo::Sha1)?;
        WritableObjectStore::write(&store, ObjectKind::Blob, b"a", WriteOptions::default())?
    };
    let oid_b = {
        let store = SqliteOdbStore::open(&path)?;
        WritableObjectStore::write(&store, ObjectKind::Blob, b"b", WriteOptions::default())?
    };
    let store = SqliteOdbStore::open(&path)?;
    assert_eq!(store.read(&oid_a)?.expect("first blob").data, b"a");
    assert_eq!(store.read(&oid_b)?.expect("second blob").data, b"b");
    Ok(())
}

#[test]
fn sqlite_object_store_example_exports_git_clean_repo() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
    let grit_log = grit_examples::sqlite_odb::run_sqlite_object_store_demo(dir.path())?;
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

//! Regression tests for FilesRefStore namespace context, refname validation,
//! prepared-commit symref races, and packed-refs cache invalidation.

use std::fs;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, FilesRefStore, FilesRefStoreConfig, RawRef, RefStore, RefStoreError, RefTransaction,
    RefUpdate, ReflogUpdate,
};
use grit_lib::refs::LogRefsConfig;
use time::OffsetDateTime;

const TRAVERSAL_REF: &str = "refs/heads/../../config";

fn sample_oid() -> ObjectId {
    "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
}

fn other_oid() -> ObjectId {
    "1111111111111111111111111111111111111111".parse().unwrap()
}

fn bare_git_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().to_path_buf();
    fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
    fs::write(
        git_dir.join("config"),
        "[core]\nrepositoryformatversion = 0\n",
    )
    .expect("config");
    (dir, git_dir)
}

fn open_namespaced(git_dir: &std::path::Path) -> FilesRefStore {
    FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.to_path_buf(),
        common_dir: git_dir.to_path_buf(),
        namespace_prefix: Some("refs/namespaces/acme/".to_owned()),
        log_refs: LogRefsConfig::Normal,
    })
}

#[test]
fn files_store_applies_explicit_namespace_prefix() {
    let (_dir, git_dir) = bare_git_dir();
    let store = open_namespaced(&git_dir);
    let oid = sample_oid();

    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: Some(RawRef::Direct(oid)),
            expected: Expected::Missing,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let namespaced = git_dir.join("refs/namespaces/acme/refs/heads/main");
    let unscoped = git_dir.join("refs/heads/main");
    assert!(
        namespaced.is_file(),
        "expected namespaced storage at {}",
        namespaced.display()
    );
    assert!(
        !unscoped.exists(),
        "logical ref must not write unscoped path {}",
        unscoped.display()
    );
    assert_eq!(
        store.read_raw("refs/heads/main").expect("read"),
        Some(RawRef::Direct(oid))
    );
}

#[test]
fn files_store_rejects_traversal_refname_before_io() {
    let (_dir, git_dir) = bare_git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let config_before = fs::read(git_dir.join("config")).expect("config");

    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: TRAVERSAL_REF.to_owned(),
            new_value: Some(RawRef::Direct(sample_oid())),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");

    let err = match store.prepare(txn) {
        Err(err) => err,
        Ok(_) => panic!("must reject traversal refname"),
    };
    assert!(
        matches!(err, RefStoreError::InvalidRefName { .. }),
        "{err:?}"
    );
    assert_eq!(
        fs::read(git_dir.join("config")).expect("config"),
        config_before,
        "repository config must not be touched"
    );
}

#[test]
fn files_store_rejects_invalid_refname_corpus() {
    let samples = [
        "refs/heads/../../config",
        "refs/heads/../../../config",
        "refs/heads/.",
        "refs/heads/..",
    ];
    let (_dir, git_dir) = bare_git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");

    for name in samples {
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: name.to_owned(),
                new_value: Some(RawRef::Direct(sample_oid())),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .expect("txn");
        let err = match store.prepare(txn) {
            Err(err) => err,
            Ok(_) => panic!("reject bad name {name}"),
        };
        assert!(
            matches!(err, RefStoreError::InvalidRefName { .. }),
            "name {name}: {err:?}"
        );
    }
}

#[test]
fn files_store_commit_uses_prepare_time_symref_target_not_raced_target() {
    let (_dir, git_dir) = bare_git_dir();
    let oid_a = sample_oid();
    let oid_b = other_oid();
    let new_oid = "2222222222222222222222222222222222222222"
        .parse::<ObjectId>()
        .unwrap();

    fs::write(git_dir.join("HEAD"), "ref: refs/heads/a\n").expect("HEAD");
    fs::write(git_dir.join("refs/heads/a"), format!("{oid_a}\n")).expect("a");
    fs::write(git_dir.join("refs/heads/b"), format!("{oid_b}\n")).expect("b");

    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(new_oid)),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");
    let prepared = store.prepare(txn).expect("prepare");

    fs::write(git_dir.join("HEAD"), "ref: refs/heads/b\n").expect("retarget HEAD");
    let b_lock = git_dir.join("refs/heads/b.lock");
    fs::write(&b_lock, "held-by-other-writer\n").expect("b.lock");

    prepared
        .commit()
        .expect("commit must use frozen prepare targets");

    assert_eq!(
        fs::read_to_string(&b_lock).expect("b.lock"),
        "held-by-other-writer\n",
        "must not clobber an unheld lock path"
    );
    assert_eq!(
        fs::read_to_string(git_dir.join("refs/heads/b")).expect("b"),
        format!("{oid_b}\n"),
        "raced symref target must be unchanged"
    );
    assert_eq!(
        fs::read_to_string(git_dir.join("refs/heads/a")).expect("a"),
        format!("{new_oid}\n"),
        "prepare-time peeled target must be updated"
    );
}

#[test]
fn files_store_observes_external_packed_refs_without_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().to_path_buf();
    let init = Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&git_dir)
        .status()
        .expect("git init");
    assert!(init.success(), "git init --bare failed");

    let oid = sample_oid();
    fs::write(git_dir.join("refs/heads/main"), format!("{oid}\n")).expect("loose main");

    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    assert_eq!(
        store.read_raw("refs/heads/main").expect("read loose"),
        Some(RawRef::Direct(oid))
    );

    let status = Command::new("git")
        .args(["pack-refs", "--all"])
        .env("GIT_DIR", &git_dir)
        .status()
        .expect("git pack-refs");
    assert!(status.success(), "git pack-refs failed");
    assert!(
        git_dir.join("packed-refs").is_file(),
        "expected packed-refs after pack-refs"
    );

    assert_eq!(
        store.read_raw("refs/heads/main").expect("read packed"),
        Some(RawRef::Direct(oid)),
        "long-lived store must reload packed-refs after external rewrite"
    );
}

#[test]
fn files_store_rejects_stale_cas_after_interleaved_commit() {
    let (_dir, git_dir) = bare_git_dir();
    let oid_old = sample_oid();
    let oid_new = other_oid();
    fs::write(git_dir.join("refs/heads/target"), format!("{oid_old}\n")).expect("seed");

    let store_a = FilesRefStore::from_git_dir(&git_dir).expect("store a");
    let store_b = FilesRefStore::from_git_dir(&git_dir).expect("store b");

    let txn_a = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/target".to_owned(),
            new_value: Some(RawRef::Direct(oid_new)),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn a");
    store_a
        .prepare(txn_a)
        .expect("prepare a")
        .commit()
        .expect("commit a");

    let txn_b = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/target".to_owned(),
            new_value: Some(RawRef::Direct(oid_old)),
            expected: Expected::Oid(oid_old),
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn b");
    let err = match store_b.prepare(txn_b) {
        Err(err) => err,
        Ok(prepared) => {
            let _ = prepared.commit();
            panic!("stale CAS must be rejected at prepare");
        }
    };
    assert!(
        matches!(err, RefStoreError::ExpectedMismatch { .. }),
        "expected ExpectedMismatch, got {err:?}"
    );
}

#[test]
fn files_store_reflog_identity_with_digits_in_name_gets_timestamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().to_path_buf();
    let init = Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&git_dir)
        .status()
        .expect("git init");
    assert!(init.success(), "git init --bare failed");

    let oid_a = sample_oid();
    let oid_b = other_oid();
    fs::write(git_dir.join("refs/heads/main"), format!("{oid_a}\n")).expect("main");

    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let time =
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid unix time for test");
    let identity = "Alice2 <alice@example.com>";
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: Some(RawRef::Direct(oid_b)),
            expected: Expected::Any,
            reflog: Some(ReflogUpdate {
                identity: identity.to_owned(),
                message: String::new(),
                time,
            }),
            flags: Default::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let log_path = git_dir.join("logs/refs/heads/main");
    let content = fs::read_to_string(&log_path).expect("reflog");
    let line = content.lines().next().expect("one line");
    assert!(
        line.contains("1700000000"),
        "reflog line must include unix time: {line}"
    );
    assert!(
        line.contains(identity),
        "reflog line must preserve name/email: {line}"
    );

    let mut count = 0;
    store
        .for_each_reflog_entry("refs/heads/main", false, &mut |entry| {
            assert!(
                entry.identity.contains("1700000000"),
                "stored identity must include unix time: {}",
                entry.identity
            );
            count += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("read reflog via store");
    assert_eq!(count, 1, "expected one reflog entry");
}

#[test]
fn files_store_observes_new_packed_refs_file_without_reopen() {
    let (_dir, git_dir) = bare_git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    assert_eq!(store.read_raw("refs/heads/main").expect("missing"), None);

    let oid = sample_oid();
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    packed.push_str(&format!("{oid} refs/heads/main\n"));
    fs::write(git_dir.join("packed-refs"), packed).expect("packed-refs");

    assert_eq!(
        store.read_raw("refs/heads/main").expect("read new packed"),
        Some(RawRef::Direct(oid))
    );
}

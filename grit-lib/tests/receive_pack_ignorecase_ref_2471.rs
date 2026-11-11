//! Issue #2471: with `core.ignorecase`, receive-pack must not create a ref whose name
//! case-folds to an existing packed ref (macOS APFS collision).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, FilesRefStore, FilesRefStoreConfig, RawRef, RefNameRef, RefStore, RefStoreError,
    RefTransaction, RefUpdate,
};
use grit_lib::refs::PackedRefRecord;
use grit_lib::remote::serve::{
    ProtocolVersion, ReceivePack, ReceivePolicy, RefUpdateRefusal, ServeOptions,
};
use grit_lib::repo::Repository;
use grit_lib::test_support::refs::write_packed_refs_file;
use grit_test_support::{git, rev_parse};
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

fn bare_with_ignorecase(path: &Path) -> Repository {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "--bare", "."]);
    git(path, &["config", "core.ignorecase", "true"]);
    let mut env = Environment::empty();
    env.cwd = path.to_path_buf();
    Repository::open(path, &RepositoryOptions::with_environment(env)).expect("open")
}

fn seed_packed_foo(path: &Path, tip: ObjectId) {
    write_packed_refs_file(
        path,
        &[PackedRefRecord {
            refname: "refs/heads/Foo".to_owned(),
            oid: tip,
            peeled: None,
        }],
    )
    .expect("packed-refs");
}

fn pack_for(source: &Path, revs: &[String]) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(source)
        .args(["pack-objects", "--revs", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects");
    let input: String = revs.iter().map(|r| format!("{r}\n")).collect();
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write revs");
    let out = child.wait_with_output().expect("pack-objects wait");
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

#[test]
fn store_rejects_casefold_create_against_packed_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path();
    git(git_dir, &["init", "-q", "--bare", "."]);
    git(git_dir, &["config", "core.ignorecase", "true"]);
    let tip = oid(0xab);
    seed_packed_foo(git_dir, tip);

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.to_path_buf(),
        common_dir: git_dir.to_path_buf(),
        namespace_prefix: None,
    });
    let update = RefUpdate::new("refs/heads/foo".to_owned(), Some(RawRef::Direct(oid(0xcd))))
        .with_expected(Expected::Missing);
    let txn = RefTransaction::new().update(update).expect("valid update");
    assert!(matches!(
        store.prepare(txn),
        Err(RefStoreError::CreateExists { ref name }) if name == "refs/heads/foo"
    ));
    assert!(
        !git_dir.join("refs/heads/foo").exists(),
        "rejected create must not write a loose ref"
    );
    assert_eq!(
        store
            .read_raw(RefNameRef::from_trusted("refs/heads/Foo"))
            .expect("read")
            .unwrap(),
        RawRef::Direct(tip)
    );
}

#[test]
fn receive_pack_reports_reference_already_exists_for_casefold_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bare = dir.path().join("srv.git");
    let repo = bare_with_ignorecase(&bare);
    let source = dir.path().join("work");
    std::fs::create_dir_all(&source).expect("mkdir");
    git(&source, &["init", "-q"]);
    git(&source, &["commit", "-q", "--allow-empty", "-m", "a"]);
    git(&source, &["commit", "-q", "--allow-empty", "-m", "b"]);
    let old = rev_parse(&source, "HEAD~1");
    let new = rev_parse(&source, "HEAD");
    seed_packed_foo(&bare, old);

    let pack = pack_for(&source, &[new.to_hex()]);
    let mut body = Vec::new();
    body.extend(pkt_line(&format!(
        "{ZERO} {} refs/heads/foo\0report-status\n",
        new.to_hex()
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);

    let mut report = Vec::new();
    let outcome = ReceivePack::new(
        &repo,
        &ServeOptions::default()
            .with_protocol(ProtocolVersion::V0)
            .with_stateless_rpc(true)
            .with_agent("grit-test".to_owned()),
        &ReceivePolicy::default(),
    )
    .with_hook(&())
    .run(&mut Cursor::new(body), &mut report)
    .expect("receive_pack");
    assert_eq!(
        outcome.updates[0].error,
        Some(RefUpdateRefusal::ReferenceAlreadyExists)
    );
    let text = String::from_utf8_lossy(&report);
    assert!(
        text.contains("ng refs/heads/foo reference already exists"),
        "{text}"
    );
    git(&bare, &["fsck", "--strict", "--no-progress"]);
    let listed = git_output(&bare, &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(listed.trim(), "refs/heads/Foo");
}

#[test]
fn initial_transaction_rejects_same_batch_casefold_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path();
    git(git_dir, &["init", "-q", "--bare", "."]);
    git(git_dir, &["config", "core.ignorecase", "true"]);
    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.to_path_buf(),
        common_dir: git_dir.to_path_buf(),
        namespace_prefix: None,
    });
    let foo = RefUpdate::new("refs/heads/Foo".to_owned(), Some(RawRef::Direct(oid(1))))
        .with_expected(Expected::Missing);
    let bar = RefUpdate::new("refs/heads/foo".to_owned(), Some(RawRef::Direct(oid(2))))
        .with_expected(Expected::Missing);
    let txn = RefTransaction::new()
        .initial()
        .update(foo)
        .expect("Foo")
        .update(bar)
        .expect("foo");
    assert!(
        matches!(store.prepare(txn), Err(RefStoreError::CaseConflict { .. })),
        "receive-pack initial batch must not accept casefold pair"
    );
}

#[test]
fn case_sensitive_config_allows_distinct_case_ref_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path();
    git(git_dir, &["init", "-q", "--bare", "."]);
    git(git_dir, &["config", "core.ignorecase", "false"]);
    seed_packed_foo(git_dir, oid(1));

    let store = FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.to_path_buf(),
        common_dir: git_dir.to_path_buf(),
        namespace_prefix: None,
    });
    let update = RefUpdate::new("refs/heads/foo".to_owned(), Some(RawRef::Direct(oid(2))))
        .with_expected(Expected::Missing);
    let txn = RefTransaction::new()
        .initial()
        .update(update)
        .expect("valid update");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("distinct spellings on case-sensitive config");
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {:?}", args);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

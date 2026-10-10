//! Round-trip checks between [`LooseStore`] and the system `git` binary.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::ops::ControlFlow;
use std::process::{Command, Stdio};

use flate2::Compression;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{LooseStore, ObjectStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_test_support::git_supports_sha256;
use grit_test_support::objects::{
    git_cat_file_batch_check, git_fsck, HashAlgo as FixtureAlgo, RepoFixture,
};

fn fixture_algo(algo: HashAlgo) -> FixtureAlgo {
    match algo {
        HashAlgo::Sha1 => FixtureAlgo::Sha1,
        HashAlgo::Sha256 => FixtureAlgo::Sha256,
    }
}

fn algos() -> Vec<HashAlgo> {
    let mut v = vec![HashAlgo::Sha1];
    if git_supports_sha256() {
        v.push(HashAlgo::Sha256);
    }
    v
}

fn init_repo(algo: HashAlgo) -> RepoFixture {
    RepoFixture::init(fixture_algo(algo)).expect("init repo")
}

#[test]
fn loose_store_writes_pass_git_fsck_and_cat_file() {
    for algo in algos() {
        let repo = init_repo(algo);
        let store = LooseStore::new(
            repo.objects_dir().to_path_buf(),
            algo,
            Compression::default(),
        );
        let payload = b"grit loose store git compat";
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, payload, WriteOptions::default())
                .expect("write");
        let fsck = git_fsck(repo.path(), true);
        assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
        let batch = git_cat_file_batch_check(repo.path(), &[&oid.to_hex()]);
        assert!(batch.ok, "batch-check: {:?}", batch.lines);
        let cat = Command::new("git")
            .current_dir(repo.path())
            .args(["cat-file", "-p", &oid.to_hex()])
            .output()
            .expect("cat-file");
        assert!(cat.status.success(), "cat-file -p failed");
        assert_eq!(
            String::from_utf8_lossy(&cat.stdout),
            "grit loose store git compat"
        );
    }
}

#[test]
fn git_loose_objects_read_through_loose_store() {
    for algo in algos() {
        let repo = init_repo(algo);
        let out = Command::new("git")
            .current_dir(repo.path())
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("hash-object");
        {
            let mut stdin = out.stdin.as_ref().expect("stdin");
            stdin.write_all(b"from git").expect("write stdin");
        }
        let out = out.wait_with_output().expect("wait");
        assert!(out.status.success());
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let oid = hex.parse().expect("oid");
        let store = LooseStore::new(
            repo.objects_dir().to_path_buf(),
            algo,
            Compression::default(),
        );
        let obj = ObjectStore::read(&store, &oid).expect("read").expect("hit");
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, b"from git");
    }
}

#[test]
fn for_each_object_matches_git_batch_check_on_loose_only_repo() {
    for algo in algos() {
        let repo = init_repo(algo);
        let store = LooseStore::new(
            repo.objects_dir().to_path_buf(),
            algo,
            Compression::default(),
        );
        for (kind, data) in [
            (ObjectKind::Blob, b"one".as_slice()),
            (ObjectKind::Blob, b"two"),
            (ObjectKind::Tree, b""),
        ] {
            WritableObjectStore::write(&store, kind, data, WriteOptions::default()).expect("write");
        }
        let mut grit_ids = HashSet::new();
        store
            .for_each_object(&mut |oid| {
                grit_ids.insert(oid.to_hex());
                ControlFlow::Continue(())
            })
            .expect("for_each");
        let git_lines = git_batch_all_objects_batch_check(repo.path());
        let git_ids: HashSet<String> = git_lines
            .iter()
            .filter_map(|line| line.split_whitespace().next().map(str::to_string))
            .collect();
        assert_eq!(grit_ids, git_ids, "loose object id sets differ");
    }
}

fn git_batch_all_objects_batch_check(repo: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .output()
        .expect("git cat-file batch-all");
    assert!(out.status.success(), "git batch-all failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn sixteen_mib_blob_streams_with_peak_buffer_under_one_mib() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LooseStore::new(
        dir.path().join("objects"),
        HashAlgo::Sha1,
        Compression::default(),
    );
    std::fs::create_dir_all(store.objects_dir()).expect("mkdir");
    let payload = vec![0xABu8; 16 * 1024 * 1024];
    let oid =
        WritableObjectStore::write(&store, ObjectKind::Blob, &payload, WriteOptions::default())
            .expect("write");
    let mut stream = ObjectStore::open_stream(&store, &oid)
        .expect("open")
        .expect("hit");
    assert_eq!(stream.size, payload.len() as u64);
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut peak = 0usize;
    loop {
        let n = stream.reader.read(&mut chunk).expect("read chunk");
        if n == 0 {
            break;
        }
        peak = peak.max(n);
        out.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(out, payload);
    assert!(
        peak < 1024 * 1024,
        "peak in-flight buffer {peak} must stay under 1 MiB"
    );
}

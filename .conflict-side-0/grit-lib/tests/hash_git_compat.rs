//! [`HashAlgo::hash_object`] compatibility with the system `git hash-object`.

use std::path::Path;

use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

fn git_hash_object(dir: &Path, kind: &str, data: &[u8]) -> String {
    let path = dir.join("hash-input.bin");
    std::fs::write(&path, data).expect("write hash input");
    let path_arg = path
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    git_cmd(&["hash-object", "-t", kind, &path_arg])
        .in_dir(dir)
        .suc()
        .stdout
        .trim()
        .to_string()
}

fn init_repo(algo: HashAlgo) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    if algo == HashAlgo::Sha256 {
        git(
            dir.path(),
            &["init", "--object-format=sha256", "--initial-branch=main"],
        );
    } else {
        git(dir.path(), &["init", "--initial-branch=main"]);
    }
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    dir
}

fn blob_sizes() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![b'x'],
        vec![0u8; 64 * 1024],
        vec![0u8; 8 * 1024 * 1024],
    ]
}

fn sample_tree_payload(_dir: &Path) -> Vec<u8> {
    Vec::new()
}

fn git_trim(dir: &Path, args: &[&str]) -> String {
    git(dir, args).trim().to_string()
}

fn sample_commit_payload(dir: &Path) -> Vec<u8> {
    let tree = git_hash_object(dir, "tree", b"");
    let commit_oid = git_trim(
        dir,
        &["commit-tree", tree.as_str(), "-m", "hash compat commit"],
    );
    git_trim(dir, &["cat-file", "-p", commit_oid.as_str()]).into_bytes()
}

fn sample_tag_payload(dir: &Path) -> Vec<u8> {
    let tree = git_hash_object(dir, "tree", b"");
    let commit_oid = git_trim(dir, &["commit-tree", tree.as_str(), "-m", "tag target"]);
    git(
        dir,
        &["tag", "-a", "v1.0", "-m", "annotated", commit_oid.as_str()],
    );
    let tag_oid = git_trim(dir, &["rev-parse", "v1.0"]);
    git_trim(dir, &["cat-file", "-p", tag_oid.as_str()]).into_bytes()
}

fn assert_kind_matches_git(dir: &Path, algo: HashAlgo, kind: ObjectKind, payload: &[u8]) {
    let git_kind = kind.as_str();
    let git_oid = git_hash_object(dir, git_kind, payload);
    let grit_oid = algo.hash_object(kind, payload).to_hex();
    assert_eq!(
        grit_oid,
        git_oid,
        "{git_kind} payload len {}",
        payload.len()
    );
}

#[test]
fn hash_object_matches_git_sha1() {
    let repo = init_repo(HashAlgo::Sha1);
    let dir = repo.path();
    for data in blob_sizes() {
        assert_kind_matches_git(dir, HashAlgo::Sha1, ObjectKind::Blob, &data);
    }
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha1,
        ObjectKind::Tree,
        &sample_tree_payload(dir),
    );
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha1,
        ObjectKind::Commit,
        &sample_commit_payload(dir),
    );
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha1,
        ObjectKind::Tag,
        &sample_tag_payload(dir),
    );
}

#[test]
fn hash_object_matches_git_sha256() {
    let repo = init_repo(HashAlgo::Sha256);
    let dir = repo.path();
    for data in blob_sizes() {
        assert_kind_matches_git(dir, HashAlgo::Sha256, ObjectKind::Blob, &data);
    }
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha256,
        ObjectKind::Tree,
        &sample_tree_payload(dir),
    );
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha256,
        ObjectKind::Commit,
        &sample_commit_payload(dir),
    );
    assert_kind_matches_git(
        dir,
        HashAlgo::Sha256,
        ObjectKind::Tag,
        &sample_tag_payload(dir),
    );
}

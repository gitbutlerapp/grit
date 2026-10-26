//! Loading large Git-written indexes: memory-mapped reads, trailer verification that runs
//! alongside entry parsing, and corruption reporting on that path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::{index_file_mtime, Index, IndexParseContext};
use grit_lib::objects::HashAlgo;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;

use support::{assert_grit_index_matches_git_ls_files, git, populate_flat_files};

/// Comfortably above the sizes at which the index is mapped and verified on a second thread.
const LARGE_INDEX_MIN_BYTES: u64 = 300 * 1024;

fn init_large_repo(root: &Path, extra_config: &[(&str, &str)]) {
    git(root, &["init", "-q", "-b", "main", "."]);
    for (key, value) in extra_config {
        git(root, &["config", key, value]);
    }
    populate_flat_files(root, "file-with-a-longer-name-", 4500);
    git(root, &["add", "-A"]);
    let len = fs::metadata(root.join(".git/index")).expect("index").len();
    assert!(
        len >= LARGE_INDEX_MIN_BYTES,
        "fixture index is only {len} bytes"
    );
}

fn assert_checksum_mismatch(result: grit_lib::error::Result<Index>) {
    match result {
        Err(Error::IndexError(msg)) => assert!(
            msg.contains("checksum mismatch"),
            "expected a checksum mismatch, got {msg}"
        ),
        Err(other) => panic!("expected IndexError, got {other:?}"),
        Ok(_) => panic!("corrupt index loaded"),
    }
}

#[test]
fn large_git_index_loads_and_matches_ls_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_large_repo(root, &[]);
    let index_path = root.join(".git/index");

    let index = Index::load(&index_path).expect("load");
    assert_eq!(index.entries().len(), 4500);
    assert_grit_index_matches_git_ls_files(root, &index);
    assert_eq!(index.source_mtime, index_file_mtime(&index_path));

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let loaded = repo.index_handle().load().expect("repository load");
    assert_grit_index_matches_git_ls_files(root, &loaded);
    assert_eq!(loaded.source_mtime, index_file_mtime(&index_path));

    let expanded = Index::load_expand_sparse(&index_path, repo.odb()).expect("expand load");
    assert_eq!(expanded.entries(), index.entries());
    assert_eq!(expanded.source_mtime, index_file_mtime(&index_path));
}

#[test]
fn large_index_with_corrupt_trailer_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_large_repo(root, &[]);
    let index_path = root.join(".git/index");
    let mut bytes = fs::read(&index_path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&index_path, &bytes).expect("write");

    assert_checksum_mismatch(Index::load(&index_path));
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_checksum_mismatch(repo.index_handle().load());
    assert_checksum_mismatch(Index::parse(&bytes));
}

#[test]
fn large_index_reports_checksum_mismatch_before_structural_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_large_repo(root, &[]);
    let index_path = root.join(".git/index");
    let mut bytes = fs::read(&index_path).expect("read");
    bytes[..4].copy_from_slice(b"XXXX");
    fs::write(&index_path, &bytes).expect("write");

    assert_checksum_mismatch(Index::load(&index_path));
    for threads in [None, Some(1), Some(4)] {
        let context = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(threads));
        assert_checksum_mismatch(Index::parse_with_algo_and_config(
            &bytes,
            HashAlgo::Sha1,
            None,
            context,
        ));
    }
}

#[test]
fn large_index_serial_thread_policy_parses_identically() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_large_repo(root, &[]);
    let bytes = fs::read(root.join(".git/index")).expect("read");

    let default = Index::parse(&bytes).expect("parse");
    let serial = Index::parse_with_algo_and_config(
        &bytes,
        HashAlgo::Sha1,
        None,
        IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(1))),
    )
    .expect("serial parse");
    assert_eq!(default.entries(), serial.entries());
    assert_eq!(default.cache_tree_root, serial.cache_tree_root);
    assert_grit_index_matches_git_ls_files(root, &serial);
}

#[test]
fn large_index_written_with_skip_hash_loads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_large_repo(root, &[("index.skipHash", "true")]);
    let index_path = root.join(".git/index");
    let bytes = fs::read(&index_path).expect("read");
    let trailer = &bytes[bytes.len() - HashAlgo::Sha1.len()..];
    if trailer.iter().any(|&b| b != 0) {
        // This git predates index.skipHash; the default path is covered above.
        return;
    }

    let index = Index::load(&index_path).expect("load skip-hash index");
    assert_grit_index_matches_git_ls_files(root, &index);
}

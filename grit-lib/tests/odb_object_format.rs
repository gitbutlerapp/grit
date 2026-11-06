//! [`Repository::open`] with an injected object store checks format against the repository's
//! `extensions.objectformat` and fails with [`RepositoryError::ObjectFormatMismatch`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, Result};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};

const MISMATCHED_PAIRS: [(HashAlgo, HashAlgo); 2] = [
    (HashAlgo::Sha256, HashAlgo::Sha1),
    (HashAlgo::Sha1, HashAlgo::Sha256),
];

fn init_with_format(root: &Path, format: HashAlgo) -> PathBuf {
    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_object_format(format),
    )
    .expect("init");
    root.join(".git")
}

fn open_with_primary(git_dir: &Path, primary: &Arc<MemoryStore>) -> Result<Repository> {
    Repository::open(
        git_dir,
        &(RepositoryOptions::empty()
            .with_odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(primary.clone())
                    .alternates(false),
            )
            .with_work_tree(
                git_dir
                    .parent()
                    .expect("git dir has a parent work tree directory"),
            )),
    )
}

fn expect_format_mismatch(
    result: Result<Repository>,
    repository_format: HashAlgo,
    store_format: HashAlgo,
) {
    match result {
        Err(Error::Repository(RepositoryError::ObjectFormatMismatch {
            repository, store, ..
        })) => {
            assert_eq!(repository, repository_format);
            assert_eq!(store, store_format);
        }
        Err(other) => panic!("expected ObjectFormatMismatch, got {other:?}"),
        Ok(repo) => panic!(
            "open accepted a {store_format:?} store for a {repository_format:?} repository \
             (odb reports {:?})",
            repo.odb().hash_algo()
        ),
    }
}

#[test]
fn custom_primary_with_a_different_hash_algorithm_is_rejected() {
    for (repo_format, store_format) in MISMATCHED_PAIRS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = init_with_format(tmp.path(), repo_format);
        let store = Arc::new(MemoryStore::new(store_format));
        expect_format_mismatch(
            open_with_primary(&git_dir, &store),
            repo_format,
            store_format,
        );
    }
}

#[test]
fn object_format_mismatch_message_names_both_algorithms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = init_with_format(tmp.path(), HashAlgo::Sha256);
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let message = open_with_primary(&git_dir, &store)
        .err()
        .expect("mismatch")
        .to_string();
    assert!(message.contains("hashes with sha1"), "{message}");
    assert!(message.contains("object format is sha256"), "{message}");
}

#[test]
fn custom_primary_with_the_repository_hash_algorithm_opens() {
    for format in [HashAlgo::Sha1, HashAlgo::Sha256] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = init_with_format(tmp.path(), format);
        let store = Arc::new(MemoryStore::new(format));
        let repo = open_with_primary(&git_dir, &store).expect("matching formats");
        assert_eq!(repo.odb().hash_algo(), format);
        let oid = repo.odb().write(ObjectKind::Blob, b"hi\n").expect("write");
        assert_eq!(oid.algo(), format);
        assert!(store.contains(&oid).expect("contains"));
    }
}

#[test]
fn custom_primary_without_config_follows_the_sha1_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path().join("nowhere.git");
    let open = |format: HashAlgo| {
        Repository::open(
            &git_dir,
            &(RepositoryOptions::empty()
                .with_odb_builder(
                    OdbBuilder::files(git_dir.join("objects"))
                        .primary(Arc::new(MemoryStore::new(format)))
                        .alternates(false),
                )
                .with_ref_store(Arc::new(MemoryRefStore::new()))
                .with_initial_branch("main")),
        )
    };
    let repo = open(HashAlgo::Sha1).expect("sha1 store without config");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha1);
    expect_format_mismatch(open(HashAlgo::Sha256), HashAlgo::Sha1, HashAlgo::Sha256);
}

#[test]
fn read_source_with_a_different_hash_algorithm_is_rejected() {
    for (repo_format, source_format) in MISMATCHED_PAIRS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = init_with_format(tmp.path(), repo_format);
        let open = |source: HashAlgo| {
            Repository::open(
                &git_dir,
                &(RepositoryOptions::empty()
                    .with_odb_builder(
                        OdbBuilder::files(git_dir.join("objects"))
                            .push_read_source(Arc::new(MemoryStore::new(repo_format)))
                            .push_read_source(Arc::new(MemoryStore::new(source))),
                    )
                    .with_work_tree(tmp.path())),
            )
        };
        expect_format_mismatch(open(source_format), repo_format, source_format);
        let repo = open(repo_format).expect("matching read sources");
        assert_eq!(repo.odb().hash_algo(), repo_format);
    }
}

#[test]
fn files_primary_opens_a_sha256_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = init_with_format(tmp.path(), HashAlgo::Sha256);
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_odb_builder(OdbBuilder::files(git_dir.join("objects")))
            .with_work_tree(tmp.path())),
    )
    .expect("files primary");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
    let oid = repo.odb().write(ObjectKind::Blob, b"hi\n").expect("write");
    assert_eq!(oid.algo(), HashAlgo::Sha256);
}

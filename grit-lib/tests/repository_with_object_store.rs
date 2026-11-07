//! [`RepositoryOptions::with_object_store`]: a custom primary object store chosen at open time,
//! with [`OdbBuilder`] reduced to read layers, an attached commit-graph and alternates.
//!
//! `Repository::open` resolves the objects directory itself (including
//! `GIT_OBJECT_DIRECTORY`), so a custom store neither names nor needs that directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};

/// An initialized non-bare repository under a fresh tempdir; returns its canonical work tree.
fn init_repo(dir: &tempfile::TempDir, format: HashAlgo) -> PathBuf {
    let root = dir.path().canonicalize().expect("canonical tempdir");
    Repository::init(
        &root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_object_format(format),
    )
    .expect("init");
    root
}

fn files_in(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    files_in(&path)
                } else {
                    1
                }
            })
            .sum()
    })
}

#[test]
fn memory_store_opens_a_repository_without_an_objects_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = init_repo(&dir, HashAlgo::Sha1);
    let git_dir = root.join(".git");
    fs::remove_dir_all(git_dir.join("objects")).expect("remove objects");

    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(&root),
    )
    .expect("open over a memory store");

    assert!(!repo.odb().uses_files_primary());
    assert_eq!(repo.odb().files_objects_dir(), None);
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha1);
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, b"memory only\n")
        .expect("write");
    assert!(store.contains(&oid).expect("store contains"));
    assert_eq!(repo.odb().read(&oid).expect("read").data, b"memory only\n");
    assert!(
        !git_dir.join("objects").exists(),
        "a custom primary must not create objects/"
    );
}

#[test]
fn memory_store_and_memory_refs_open_a_missing_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("absent.git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_initial_branch("main"),
    )
    .expect("open with both stores injected");
    let oid = repo.odb().write(ObjectKind::Blob, b"x").expect("write");
    assert!(store.contains(&oid).expect("contains"));
    assert!(!git_dir.exists());
}

#[test]
fn files_primary_still_requires_its_objects_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = init_repo(&dir, HashAlgo::Sha1);
    let git_dir = root.join(".git");
    fs::remove_dir_all(git_dir.join("objects")).expect("remove objects");
    for options in [
        RepositoryOptions::empty(),
        RepositoryOptions::empty().with_odb_builder(OdbBuilder::new()),
    ] {
        let err = Repository::open(&git_dir, &options.with_work_tree(&root))
            .err()
            .expect("files primary needs objects/");
        assert!(
            matches!(err, Error::Repository(RepositoryError::NotARepository(_))),
            "{err:?}"
        );
    }
}

/// With `GIT_OBJECT_DIRECTORY` set, the custom store still wins: objects go to the store and
/// the environment's directory is left exactly as it was.
#[test]
fn custom_store_wins_over_git_object_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = init_repo(&dir, HashAlgo::Sha1);
    let env_objects = root.join("env-objects");
    fs::create_dir_all(&env_objects).expect("env objects dir");
    let mut environment = Environment::empty();
    environment.cwd = root.clone();
    environment.git_object_directory = Some("env-objects".to_owned());

    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::with_environment(environment.clone())
            .with_object_store(store.clone())
            .with_work_tree(&root),
    )
    .expect("open with env dir and custom store");
    assert_eq!(repo.odb().objects_dir(), env_objects.as_path());
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, b"to the store\n")
        .expect("write");
    assert!(store.contains(&oid).expect("store contains"));
    assert_eq!(files_in(&env_objects), 0, "env dir must stay untouched");
    assert_eq!(files_in(&root.join(".git/objects")), 0);

    // Without the custom store the same environment writes into the env dir.
    let files = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::with_environment(environment).with_work_tree(&root),
    )
    .expect("open files over env dir");
    files
        .odb()
        .write(ObjectKind::Blob, b"to the env dir\n")
        .expect("write");
    assert_eq!(files_in(&env_objects), 1);
}

#[test]
fn custom_store_with_another_hash_is_an_object_format_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = init_repo(&dir, HashAlgo::Sha256);
    let err = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .with_work_tree(&root),
    )
    .err()
    .expect("a SHA-1 store cannot back a SHA-256 repository");
    assert!(
        matches!(
            err,
            Error::Repository(RepositoryError::ObjectFormatMismatch {
                repository: HashAlgo::Sha256,
                store: HashAlgo::Sha1,
                ..
            })
        ),
        "{err:?}"
    );

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha256)))
            .with_work_tree(&root),
    )
    .expect("a matching SHA-256 store opens");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
}

/// `OdbBuilder::new()` layers read sources over the files primary: reads find the source's
/// objects and writes still land in `objects/`.
#[test]
fn read_sources_layer_over_the_files_primary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = init_repo(&dir, HashAlgo::Sha1);
    let source = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let shared = source
        .write(
            ObjectKind::Blob,
            b"from the source\n",
            WriteOptions::default(),
        )
        .expect("seed source");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_odb_builder(OdbBuilder::new().push_read_source(source.clone()))
            .with_work_tree(&root),
    )
    .expect("open with a read source");
    assert!(repo.odb().uses_files_primary());
    assert_eq!(
        repo.odb().read(&shared).expect("read").data,
        b"from the source\n"
    );
    let local = repo
        .odb()
        .write(ObjectKind::Blob, b"local\n")
        .expect("write");
    assert!(!source.contains(&local).expect("source contains"));
    assert!(repo.odb().object_path(&local).is_file());
}

/// `OdbBuilder::new()` layers read sources over a custom primary too.
#[test]
fn read_sources_layer_over_a_custom_primary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primary = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let source = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let shared = source
        .write(ObjectKind::Blob, b"shared\n", WriteOptions::default())
        .expect("seed source");
    let repo = Repository::open(
        &dir.path().join("layered.git"),
        &RepositoryOptions::empty()
            .with_object_store(primary.clone())
            .with_odb_builder(OdbBuilder::new().push_read_source(source.clone()))
            .with_ref_store(Arc::new(MemoryRefStore::new())),
    )
    .expect("open");
    assert!(repo.odb().contains(&shared).expect("contains"));
    let local = repo.odb().write(ObjectKind::Blob, b"new\n").expect("write");
    assert!(primary.contains(&local).expect("primary contains"));
    assert!(!source.contains(&local).expect("source contains"));
}

/// A repository whose `objects/info/alternates` names a second object directory holding one
/// blob. Returns the work tree and that blob's id.
fn repo_with_alternate(dir: &tempfile::TempDir) -> (PathBuf, grit_lib::objects::ObjectId) {
    let root = init_repo(dir, HashAlgo::Sha1);
    let alternate = root.join("alternate-objects");
    let alt_oid = Odb::new(&alternate)
        .write(ObjectKind::Blob, b"from the alternate\n")
        .expect("write alternate");
    let info = root.join(".git/objects/info");
    fs::create_dir_all(&info).expect("info dir");
    fs::write(
        info.join("alternates"),
        format!("{}\n", alternate.display()),
    )
    .expect("alternates file");
    (root, alt_oid)
}

#[test]
fn alternates_default_off_for_a_custom_store_and_on_when_requested() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, alt_oid) = repo_with_alternate(&dir);
    let open = |builder: Option<OdbBuilder>| {
        let mut options = RepositoryOptions::empty()
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .with_work_tree(&root);
        if let Some(builder) = builder {
            options = options.with_odb_builder(builder);
        }
        Repository::open(&root.join(".git"), &options).expect("open")
    };

    let default = open(None);
    assert!(!default.odb().contains(&alt_oid).expect("contains"));
    let layered_default = open(Some(OdbBuilder::new()));
    assert!(!layered_default.odb().contains(&alt_oid).expect("contains"));

    let with_alternates = open(Some(OdbBuilder::new().alternates(true)));
    assert!(with_alternates.odb().contains(&alt_oid).expect("contains"));
    assert_eq!(
        with_alternates.odb().read(&alt_oid).expect("read").data,
        b"from the alternate\n"
    );
}

#[test]
fn alternates_default_on_for_the_files_primary_and_off_when_requested() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, alt_oid) = repo_with_alternate(&dir);
    let open = |options: RepositoryOptions| {
        Repository::open(&root.join(".git"), &options.with_work_tree(&root)).expect("open")
    };

    assert!(open(RepositoryOptions::empty())
        .odb()
        .contains(&alt_oid)
        .expect("contains"));
    assert!(
        open(RepositoryOptions::empty().with_odb_builder(OdbBuilder::new()))
            .odb()
            .contains(&alt_oid)
            .expect("contains")
    );
    assert!(!open(
        RepositoryOptions::empty().with_odb_builder(OdbBuilder::new().alternates(false))
    )
    .odb()
    .contains(&alt_oid)
    .expect("contains"));
}

#[test]
fn builder_and_options_defaults() {
    let options = RepositoryOptions::empty();
    assert!(options.object_store.is_none());
    assert!(options.odb_builder.is_none());

    let store: Arc<dyn WritableObjectStore> = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let options = options
        .with_object_store(Arc::clone(&store))
        .with_odb_builder(OdbBuilder::default());
    assert!(options
        .object_store
        .as_ref()
        .is_some_and(|set| Arc::ptr_eq(set, &store)));
    assert!(options.odb_builder.is_some());
}

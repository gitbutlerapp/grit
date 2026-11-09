//! [`Repository::init`] on injected object and ref stores
//! ([`InitOptions::with_repository_options`]): the layout a store owns is never created on
//! disk, `HEAD` is seeded in an injected ref store, and the returned handle reads and writes
//! through the stores.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::porcelain::log::DecorationOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefNameRef, RefStore};
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use grit_lib::revwalk::rev_list::RevListOptions;

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

struct Stores {
    objects: Arc<MemoryStore>,
    refs: Arc<MemoryRefStore>,
}

impl Stores {
    fn new(algo: HashAlgo) -> Self {
        Self {
            objects: Arc::new(MemoryStore::new(algo)),
            refs: Arc::new(MemoryRefStore::new()),
        }
    }

    fn options(&self) -> RepositoryOptions {
        RepositoryOptions::empty()
            .with_object_store(self.objects.clone())
            .with_ref_store(self.refs.clone())
    }
}

fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn head(refs: &dyn RefStore) -> Option<RawRef> {
    refs.read_raw(RefNameRef::from_trusted("HEAD"))
        .expect("read HEAD")
}

fn request(message: &str) -> CommitRequest {
    let mut req = CommitRequest::new(message.to_owned());
    req.author = Some(IDENT.parse().expect("signature"));
    req.committer = Some(IDENT.parse().expect("signature"));
    req
}

fn commit_work_tree(repo: &Repository, message: &str) -> ObjectId {
    repo.index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage");
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(request(message), CommitScope::Staged),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit")
        .oid
}

fn commit_empty(repo: &Repository, message: &str) -> ObjectId {
    let mut req = request(message);
    req.allow_empty = true;
    repo.commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit")
        .oid
}

/// Subjects and decorations of the history reachable from `HEAD`, newest first.
fn log(repo: &Repository) -> Vec<(ObjectId, String, Vec<String>)> {
    let revwalk = repo.revwalk();
    let walked = revwalk
        .rev_list(&["HEAD".to_owned()], &[], &RevListOptions::default())
        .expect("rev-list");
    let decorations = revwalk
        .decorations(&DecorationOptions::default())
        .expect("decorations");
    revwalk
        .load_commits(&walked.commits, &decorations, &mut ())
        .expect("load commits")
        .into_iter()
        .map(|commit| {
            let subject = commit.message.lines().next().unwrap_or("").to_owned();
            let refs = commit.refs.iter().map(|r| r.name.clone()).collect();
            (commit.oid, subject, refs)
        })
        .collect()
}

#[test]
fn non_bare_init_writes_only_config_and_commits_through_the_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("work");
    let stores = Stores::new(HashAlgo::Sha1);
    let (repo, outcome) = Repository::init(
        &work,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_repository_options(stores.options()),
    )
    .expect("init");

    assert!(!outcome.reinitialized);
    assert_eq!(outcome.initial_branch, "main");
    assert_eq!(names(&work), set(&[".git"]));
    assert_eq!(names(&work.join(".git")), set(&["config"]));
    assert_eq!(
        head(stores.refs.as_ref()),
        Some(RawRef::Symbolic("refs/heads/main".to_owned()))
    );
    assert_eq!(
        repo.work_tree(),
        Some(work.canonicalize().expect("canonical").as_path())
    );

    fs::write(work.join("file.txt"), "one\n").expect("write");
    let first = commit_work_tree(&repo, "first");
    fs::write(work.join("file.txt"), "two\n").expect("write");
    let second = commit_work_tree(&repo, "second");

    assert!(stores.objects.contains(&first).expect("contains"));
    assert!(stores.objects.contains(&second).expect("contains"));
    let history = log(&repo);
    assert_eq!(
        history
            .iter()
            .map(|(oid, subject, _)| (*oid, subject.as_str()))
            .collect::<Vec<_>>(),
        vec![(second, "second"), (first, "first")]
    );
    assert!(history[0].2.iter().any(|name| name.contains("main")));

    let git_dir = work.join(".git");
    for owned_by_a_store in ["objects", "refs", "HEAD", "packed-refs", "logs"] {
        assert!(
            !git_dir.join(owned_by_a_store).exists(),
            "{owned_by_a_store} must stay in the injected stores"
        );
    }
}

#[test]
fn bare_init_writes_only_config_and_commits_through_the_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("bare.git");
    let stores = Stores::new(HashAlgo::Sha1);
    let (repo, outcome) = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("trunk")
            .with_repository_options(stores.options()),
    )
    .expect("init");

    assert_eq!(outcome.initial_branch, "trunk");
    assert_eq!(names(&git_dir), set(&["config"]));
    assert!(repo.work_tree().is_none());
    let config = fs::read_to_string(git_dir.join("config")).expect("config");
    assert!(config.contains("repositoryformatversion = 0"), "{config}");
    assert!(config.contains("bare = true"), "{config}");
    assert!(!config.contains("objectformat"), "{config}");
    assert_eq!(
        head(stores.refs.as_ref()),
        Some(RawRef::Symbolic("refs/heads/trunk".to_owned()))
    );

    let first = commit_empty(&repo, "first");
    let second = commit_empty(&repo, "second");
    assert!(stores.objects.contains(&second).expect("contains"));
    let history = log(&repo);
    assert_eq!(
        history
            .iter()
            .map(|(oid, subject, _)| (*oid, subject.as_str()))
            .collect::<Vec<_>>(),
        vec![(second, "second"), (first, "first")]
    );
    assert!(!git_dir.join("objects").exists());
    assert!(!git_dir.join("refs").exists());
    assert!(!git_dir.join("HEAD").exists());
}

#[test]
fn sha256_store_sets_the_object_format() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("sha256.git");
    let stores = Stores::new(HashAlgo::Sha256);
    let (repo, _) = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_repository_options(stores.options()),
    )
    .expect("init");

    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
    assert_eq!(names(&git_dir), set(&["config"]));
    let config = repo.config().snapshot().expect("config");
    assert_eq!(
        config
            .get("extensions.objectformat")
            .expect("get")
            .as_deref(),
        Some("sha256")
    );
    assert_eq!(
        config
            .get("core.repositoryformatversion")
            .expect("get")
            .as_deref(),
        Some("1")
    );

    let commit = commit_empty(&repo, "sha256 root");
    assert_eq!(commit.as_bytes().len(), 32);
    assert!(stores.objects.contains(&commit).expect("contains"));
    assert_eq!(log(&repo)[0].0, commit);
}

#[test]
fn matching_explicit_object_format_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("explicit.git");
    let stores = Stores::new(HashAlgo::Sha256);
    let (repo, _) = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_object_format(HashAlgo::Sha256)
            .with_repository_options(stores.options()),
    )
    .expect("init");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
}

#[test]
fn object_format_mismatch_fails_before_writing_anything() {
    let dir = tempfile::tempdir().expect("tempdir");
    for bare in [false, true] {
        let path = dir.path().join(if bare { "bare.git" } else { "work" });
        let stores = Stores::new(HashAlgo::Sha1);
        let result = Repository::init(
            &path,
            &InitOptions::new()
                .with_bare(bare)
                .with_object_format(HashAlgo::Sha256)
                .with_repository_options(stores.options()),
        );
        assert!(
            matches!(
                result,
                Err(Error::Repository(RepositoryError::ObjectFormatMismatch {
                    repository: HashAlgo::Sha256,
                    store: HashAlgo::Sha1,
                }))
            ),
            "{result:?}"
        );
        assert!(!path.exists(), "nothing is created at {}", path.display());
        assert_eq!(head(stores.refs.as_ref()), None);
    }
    assert!(names(dir.path()).is_empty());
}

#[test]
fn reinit_with_a_store_of_another_format_fails_before_writing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("repo.git");
    let stores = Stores::new(HashAlgo::Sha1);
    Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_repository_options(stores.options()),
    )
    .expect("init");
    let config_before = fs::read(git_dir.join("config")).expect("config");

    let other = Stores::new(HashAlgo::Sha256);
    let result = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_repository_options(other.options()),
    );
    assert!(
        matches!(
            result,
            Err(Error::Repository(RepositoryError::ObjectFormatMismatch {
                repository: HashAlgo::Sha1,
                store: HashAlgo::Sha256,
            }))
        ),
        "{result:?}"
    );
    assert_eq!(
        fs::read(git_dir.join("config")).expect("config"),
        config_before
    );
    assert_eq!(head(other.refs.as_ref()), None);
}

#[test]
fn reftable_with_an_injected_ref_store_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("reftable.git");
    let stores = Stores::new(HashAlgo::Sha1);
    let result = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_ref_storage(RefStorageFormat::Reftable)
            .with_repository_options(stores.options()),
    );
    assert!(
        matches!(
            result,
            Err(Error::Repository(
                RepositoryError::InitRefStorageWithInjectedRefStore {
                    requested: RefStorageFormat::Reftable,
                }
            ))
        ),
        "{result:?}"
    );
    assert!(!git_dir.exists());
}

#[test]
fn reinit_keeps_config_and_the_stores_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("work");
    let stores = Stores::new(HashAlgo::Sha1);
    let init = || {
        Repository::init(
            &work,
            &InitOptions::new()
                .with_initial_branch("main")
                .with_repository_options(stores.options()),
        )
        .expect("init")
    };
    let (repo, _) = init();
    let config_path = work.join(".git/config");
    let mut config = fs::read_to_string(&config_path).expect("config");
    config.push_str("[user]\n\tname = Kept\n");
    fs::write(&config_path, &config).expect("config");
    fs::write(work.join("file.txt"), "content\n").expect("write");
    let tip = commit_work_tree(&repo, "first");
    stores
        .refs
        .set_ref("HEAD", RawRef::Symbolic("refs/heads/other".to_owned()))
        .expect("move HEAD");

    let (again, outcome) = init();
    assert!(outcome.reinitialized);
    assert_eq!(outcome.initial_branch, "other");
    assert_eq!(fs::read_to_string(&config_path).expect("config"), config);
    assert_eq!(
        head(stores.refs.as_ref()),
        Some(RawRef::Symbolic("refs/heads/other".to_owned()))
    );
    assert!(again.odb().contains(&tip).expect("contains"));
    assert_eq!(
        names(&work.join(".git")),
        set(&["COMMIT_EDITMSG", "config", "index"])
    );
}

#[test]
fn an_injected_object_store_alone_skips_only_objects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("work");
    let objects = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let (repo, _) = Repository::init(
        &work,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_repository_options(RepositoryOptions::empty().with_object_store(objects.clone())),
    )
    .expect("init");

    let git_dir = work.join(".git");
    assert!(!git_dir.join("objects").exists());
    assert_eq!(
        fs::read_to_string(git_dir.join("HEAD")).expect("HEAD"),
        "ref: refs/heads/main\n"
    );
    assert!(git_dir.join("refs/heads").is_dir());
    assert!(git_dir.join("description").is_file());

    fs::write(work.join("file.txt"), "content\n").expect("write");
    let commit = commit_work_tree(&repo, "first");
    assert!(objects.contains(&commit).expect("contains"));
    assert!(!git_dir.join("objects").exists());
    assert_eq!(
        fs::read_to_string(git_dir.join("refs/heads/main")).expect("branch"),
        format!("{commit}\n")
    );
}

#[test]
fn an_injected_ref_store_alone_skips_only_refs_and_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("refs-in-memory.git");
    let refs = Arc::new(MemoryRefStore::new());
    let (repo, _) = Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_repository_options(RepositoryOptions::empty().with_ref_store(refs.clone())),
    )
    .expect("init");

    assert!(git_dir.join("objects/pack").is_dir());
    assert!(git_dir.join("objects/info").is_dir());
    assert!(!git_dir.join("refs").exists());
    assert!(!git_dir.join("HEAD").exists());
    assert_eq!(
        head(refs.as_ref()),
        Some(RawRef::Symbolic("refs/heads/main".to_owned()))
    );

    let commit = commit_empty(&repo, "first");
    assert!(repo.odb().contains(&commit).expect("contains"));
    assert_eq!(log(&repo)[0].0, commit);
    assert!(!git_dir.join("refs").exists());
}

#[test]
fn repository_options_reach_the_returned_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = dir.path().join("global.cfg");
    fs::write(&global, "[init]\n\tdefaultBranch = from-env\n").expect("global");
    let environment = grit_lib::config::environment::Environment::from_vars(
        [
            ("HOME", dir.path().as_os_str()),
            ("GIT_CONFIG_NOSYSTEM", "1".as_ref()),
            ("GIT_CONFIG_GLOBAL", global.as_os_str()),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), v.to_owned())),
        dir.path().to_path_buf(),
    );
    let stores = Stores::new(HashAlgo::Sha1);
    let options = RepositoryOptions::with_environment(environment)
        .with_object_store(stores.objects.clone())
        .with_ref_store(stores.refs.clone());
    let (repo, outcome) = Repository::init(
        &dir.path().join("env.git"),
        &InitOptions::new()
            .with_bare(true)
            .with_repository_options(options),
    )
    .expect("init");
    assert_eq!(outcome.initial_branch, "from-env");
    assert_eq!(
        head(stores.refs.as_ref()),
        Some(RawRef::Symbolic("refs/heads/from-env".to_owned()))
    );
    assert_eq!(
        repo.config()
            .snapshot()
            .expect("config")
            .get("init.defaultbranch")
            .expect("get")
            .as_deref(),
        Some("from-env")
    );
}

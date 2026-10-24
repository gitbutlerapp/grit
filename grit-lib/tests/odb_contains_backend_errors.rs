//! A failing custom object store must surface as an error from existence checks,
//! not as "object missing" (issue #1751).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::{Error, Result};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::refs::refs_fsck::refs_fsck;
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_parse::abbreviate_object_id;

#[derive(Debug)]
struct Flaky {
    inner: MemoryStore,
    fail: AtomicBool,
}

impl Flaky {
    fn check(&self) -> Result<()> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "backend down",
            )));
        }
        Ok(())
    }
}

impl ObjectStore for Flaky {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.check()?;
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.check()?;
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        self.check()?;
        self.inner.contains(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.check()?;
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for Flaky {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        self.check()?;
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        self.check()?;
        self.inner.freshen(oid)
    }
}

fn is_backend_down(err: &Error) -> bool {
    matches!(err, Error::Io(e) if e.kind() == io::ErrorKind::ConnectionReset)
}

/// A repository whose primary object store is `Flaky`, holding one commit on `main`.
fn flaky_repo(dir: &Path) -> (Repository, Arc<Flaky>, ObjectId) {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.join(".git").canonicalize().expect("canonical git dir");
    let flaky = Arc::new(Flaky {
        inner: MemoryStore::new(HashAlgo::Sha1),
        fail: AtomicBool::new(false),
    });
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(flaky.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(dir))
        .expect("open");
    let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
    let commit = repo
        .odb()
        .write(
            ObjectKind::Commit,
            format!(
                "tree {}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nc\n",
                tree.to_hex()
            )
            .as_bytes(),
        )
        .expect("commit");
    write_ref(&git_dir, "refs/heads/main", &commit).expect("ref");
    (repo, flaky, commit)
}

#[test]
fn contains_reports_backend_failure_like_read_does() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, flaky, commit) = flaky_repo(dir.path());
    assert!(repo.odb().contains(&commit).unwrap());
    assert!(repo.objects().contains(&commit).unwrap());

    flaky.fail.store(true, Ordering::Relaxed);
    assert!(is_backend_down(&repo.odb().read(&commit).unwrap_err()));
    assert!(is_backend_down(&repo.odb().contains(&commit).unwrap_err()));
    assert!(is_backend_down(
        &repo.objects().contains(&commit).unwrap_err()
    ));
    assert!(
        !repo.odb().exists(&commit),
        "exists() stays the lenient probe"
    );
}

#[test]
fn refs_fsck_reports_backend_failure_not_missing_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, flaky, _commit) = flaky_repo(dir.path());
    let config = ConfigSet::load_repo_local_only(repo.git_dir()).expect("config");
    let issues = refs_fsck(&repo, repo.odb(), &config, false).expect("healthy fsck");
    assert!(
        issues.iter().all(|i| i.msg_id != "missingObject"),
        "{issues:?}"
    );

    flaky.fail.store(true, Ordering::Relaxed);
    let err = refs_fsck(&repo, repo.odb(), &config, false).unwrap_err();
    assert!(is_backend_down(&err), "{err:?}");
}

#[test]
fn abbreviate_object_id_reports_backend_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, flaky, commit) = flaky_repo(dir.path());
    assert!(abbreviate_object_id(&repo, commit, 7).is_ok());

    flaky.fail.store(true, Ordering::Relaxed);
    let err = abbreviate_object_id(&repo, commit, 7).unwrap_err();
    assert!(is_backend_down(&err), "{err:?}");
}

//! [`Repository::refs`] honors `GIT_NAMESPACE` from [`RepositoryOptions`].

use std::fs;
use std::path::PathBuf;

use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, RawRef, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::Repository;

fn sample_oid() -> ObjectId {
    "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
}

fn bare_git_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().to_path_buf();
    fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
    fs::create_dir_all(git_dir.join("objects/info")).expect("objects");
    fs::write(
        git_dir.join("config"),
        "[core]\nrepositoryformatversion = 0\nbare = true\n",
    )
    .expect("config");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    (dir, git_dir)
}

#[test]
fn repository_files_refstore_honors_git_namespace_option() {
    let (_dir, git_dir) = bare_git_dir();
    let mut env = Environment::empty();
    env.git_namespace = Some("acme".to_owned());
    let opts = RepositoryOptions::with_environment(env);
    let repo = Repository::open_with(&opts, &git_dir, None).expect("open");

    let oid = sample_oid();
    repo.refs()
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/topic".to_owned(),
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Missing,
                    reflog: None,
                    flags: RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    let namespaced = git_dir.join("refs/namespaces/acme/refs/heads/topic");
    let unscoped = git_dir.join("refs/heads/topic");
    assert!(
        namespaced.is_file(),
        "expected namespaced storage at {}",
        namespaced.display()
    );
    assert!(
        !unscoped.exists(),
        "must not write unscoped ref at {}",
        unscoped.display()
    );
}

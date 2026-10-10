//! [`Repository::refs`] honors `GIT_NAMESPACE` from [`RepositoryOptions`].

use std::collections::BTreeSet;
use std::fs;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Command;

use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::Repository;

fn sample_oid() -> ObjectId {
    "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
}

fn other_oid() -> ObjectId {
    "1111111111111111111111111111111111111111".parse().unwrap()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn head_branch_names(repo: &Repository) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    repo.refs()
        .for_each_ref("refs/heads/", &mut |entry| {
            names.insert(entry.name.clone());
            ControlFlow::Continue(())
        })
        .expect("for_each_ref");
    names
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

#[test]
fn repository_namespaced_refstore_resolves_global_head() {
    let (_dir, git_dir) = bare_git_dir();
    let oid = sample_oid();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let namespaced_main = git_dir.join("refs/namespaces/acme/refs/heads/main");
    fs::create_dir_all(namespaced_main.parent().expect("parent")).expect("mkdir");
    fs::write(namespaced_main, format!("{oid}\n")).expect("main");

    let mut env = Environment::empty();
    env.git_namespace = Some("acme".to_owned());
    let opts = RepositoryOptions::with_environment(env);
    let repo = Repository::open_with(&opts, &git_dir, None).expect("open");

    assert_eq!(repo.resolve_ref_name("refs/heads/main").expect("main"), oid);
    assert_eq!(repo.resolve_ref_name("HEAD").expect("HEAD"), oid);
}

#[test]
fn repository_namespaced_packed_refs_exclude_unscoped_branches() {
    let (_dir, git_dir) = bare_git_dir();
    let scoped = sample_oid();
    let unscoped = other_oid();
    fs::write(
        git_dir.join("packed-refs"),
        format!(
            "# pack-refs with: peeled fully-peeled sorted\n{unscoped} refs/heads/unscoped\n{scoped} refs/namespaces/acme/refs/heads/scoped\n"
        ),
    )
    .expect("packed-refs");

    let mut env = Environment::empty();
    env.git_namespace = Some("acme".to_owned());
    let opts = RepositoryOptions::with_environment(env);
    let repo = Repository::open_with(&opts, &git_dir, None).expect("open");

    let grit_heads = head_branch_names(&repo);
    assert_eq!(grit_heads, BTreeSet::from(["refs/heads/scoped".to_owned()]));

    let listed = list_refs_for_repository(&repo, "refs/heads/").expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "refs/heads/scoped");
    assert_eq!(listed[0].1, scoped);

    if !git_available() {
        return;
    }
    let output = Command::new("git")
        .env("GIT_NAMESPACE", "acme")
        .current_dir(&git_dir)
        .args(["upload-pack", "--advertise-refs", "."])
        .output()
        .expect("git upload-pack");
    assert!(
        output.status.success(),
        "git upload-pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let advert = String::from_utf8_lossy(&output.stdout);
    assert!(
        advert.contains("refs/heads/scoped"),
        "git should advertise scoped branch:\n{advert}"
    );
    assert!(
        !advert.contains("refs/heads/unscoped"),
        "git must not advertise unscoped packed branch:\n{advert}"
    );
}

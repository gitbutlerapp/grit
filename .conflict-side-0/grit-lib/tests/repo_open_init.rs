//! Open, discover, and init entry points (task 1304).

use std::process::Command;
use std::sync::OnceLock;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::RefStorageFormat;
use tempfile::TempDir;

fn git_fsck_ok(git_dir: &std::path::Path) {
    let output = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "fsck"])
        .output()
        .expect("spawn git fsck");
    assert!(
        output.status.success(),
        "git fsck failed for {}: {}",
        git_dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_supports_reftable_fsck() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q", "--ref-format=reftable"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn git_fsck_reftable_repo(git_dir: &std::path::Path) {
    let output = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "fsck"])
        .output()
        .expect("spawn git fsck");
    if output.status.success() {
        return;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if git_supports_reftable_fsck() {
        panic!(
            "git fsck failed for reftable repo {} but system git supports reftable: {}",
            git_dir.display(),
            stderr
        );
    }
    assert!(
        stderr.contains("unknown repository extension"),
        "expected unsupported reftable extension on this git, got: {stderr}"
    );
}

#[test]
fn open_discover_and_init_roundtrip_fsck() {
    let td = TempDir::new().expect("tempdir");

    let normal = td.path().join("normal");
    let repo = Repository::init(
        &normal,
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init normal");
    git_fsck_ok(repo.git_dir());

    let bare = td.path().join("bare.git");
    let bare_repo = Repository::init(
        &bare,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned()),
    )
    .expect("init bare");
    git_fsck_ok(bare_repo.git_dir());

    let sep_wt = td.path().join("sep-wt");
    let sep_git = td.path().join("sep.git");
    let sep_repo = Repository::init(
        &sep_wt,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_separate_git_dir(Some(sep_git.clone())),
    )
    .expect("init separate");
    git_fsck_ok(sep_repo.git_dir());

    let bare_wt = td.path().join("bare-wt-layout");
    let bare_wt_repo = Repository::init(
        &bare_wt,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_bare_with_worktree_at_path(true),
    )
    .expect("init bare-with-worktree-at-path");
    git_fsck_ok(bare_wt_repo.git_dir());

    let reftable = td.path().join("reftable");
    let rt_repo = Repository::init(
        &reftable,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init reftable");
    git_fsck_reftable_repo(rt_repo.git_dir());

    let reopened = Repository::open(
        repo.git_dir(),
        &RepositoryOptions::empty().with_work_tree(normal.clone()),
    )
    .expect("open explicit");
    assert_eq!(reopened.git_dir(), repo.git_dir());

    let discovered = Repository::discover(Some(&normal), &RepositoryOptions::empty())
        .expect("discover from work tree");
    assert_eq!(discovered.git_dir(), repo.git_dir());

    let info = discovered.discovery_info();
    assert!(info.discovery_root.is_some());
    assert!(!info.explicit_git_dir);

    let serving = Repository::open(
        bare_repo.git_dir(),
        &RepositoryOptions::empty().for_serving(),
    )
    .expect("open for serving");
    assert_eq!(serving.git_dir(), bare_repo.git_dir());

    let skip_fmt = Repository::open(
        repo.git_dir(),
        &RepositoryOptions::empty()
            .with_work_tree(normal.clone())
            .without_format_validation(),
    )
    .expect("open skipping format validation");
    assert_eq!(skip_fmt.git_dir(), repo.git_dir());

    let oid: ObjectId = "0100000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let store = std::sync::Arc::new(MemoryRefStore::new());
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/builder-injected".to_owned(),
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Missing,
                    reflog: None,
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    let built = RepositoryBuilder::new(RepositoryOptions::empty().with_work_tree(normal.clone()))
        .ref_store(store)
        .open(repo.git_dir(), Some(normal.as_path()))
        .expect("builder open");
    assert_eq!(built.git_dir(), repo.git_dir());
    assert_eq!(
        built.refs().format(),
        grit_lib::refs::store::RefStorageFormat::Memory
    );
    let listed = list_refs_for_repository(&built, "refs/heads/").expect("list");
    assert!(
        listed
            .iter()
            .any(|(name, id)| name == "refs/heads/builder-injected" && *id == oid),
        "builder ref_store injection should list injected ref"
    );
    assert_eq!(
        resolve_revision(&built, "refs/heads/builder-injected").expect("rev-parse"),
        oid
    );
}

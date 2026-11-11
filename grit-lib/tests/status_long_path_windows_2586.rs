//! Issue #2586: after clone, status must not report an existing >260-char path as deleted.

#![cfg(windows)]

use grit_lib::objects::FileMode;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diff::TreeDiffOptions;
use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectKind, TreeEntry};
use grit_lib::porcelain::status::{StatusOptions, UntrackedMode};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::RefName;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;
use time::OffsetDateTime;

fn component_d() -> Vec<u8> {
    vec![b'd'; 120]
}

fn build_bare_source(tmp: &Path) -> PathBuf {
    let bare = tmp.join("src.git");
    std::fs::create_dir_all(&bare).expect("bare root");
    let options = InitOptions::new()
        .with_bare(true)
        .with_initial_branch("main".to_owned())
        .with_ref_storage(RefStorageFormat::Files);
    let (repo, _) = Repository::init(&bare, &options).expect("init bare");
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, b"hi\n")
        .expect("blob");
    let ok_blob = repo
        .objects()
        .write(ObjectKind::Blob, b"ok\n")
        .expect("ok blob");
    let t1 = repo
        .objects()
        .write_tree([TreeEntry::new(b"x.txt", FileMode::REGULAR, blob)])
        .expect("t1");
    let d = component_d();
    let t2 = repo
        .objects()
        .write_tree([TreeEntry::new(d.clone(), FileMode::TREE, t1)])
        .expect("t2");
    let t3 = repo
        .objects()
        .write_tree([TreeEntry::new(d.clone(), FileMode::TREE, t2)])
        .expect("t3");
    let t4 = repo
        .objects()
        .write_tree([TreeEntry::new(d, FileMode::TREE, t3)])
        .expect("t4");
    let root = repo
        .objects()
        .write_tree([
            TreeEntry::new(b"ok.txt", FileMode::REGULAR, ok_blob),
            TreeEntry::new(b"long", FileMode::TREE, t4),
        ])
        .expect("root");
    let sig = Signature::new(
        "D",
        "d@x.org",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("sig");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(root, sig.clone(), sig, "msg"))
        .expect("commit");
    let main = RefName::try_from("refs/heads/main").expect("ref");
    repo.references()
        .update(&main, &commit)
        .expect("update main");
    bare
}

fn long_leaf_path(work: &Path) -> PathBuf {
    let d = String::from_utf8(component_d()).expect("ascii");
    work.join("long").join(&d).join(&d).join(&d).join("x.txt")
}

fn git_longpaths(dir: &Path, args: &[&str]) -> String {
    let mut argv = vec!["-c", "core.longpaths=true"];
    argv.extend(args);
    git(dir, &argv)
}

fn open_worktree(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn assert_grit_worktree_diff_empty(root: &Path) {
    let repo = open_worktree(root);
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("load index");
    let entries = repo
        .diff()
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("index_to_worktree diff");
    assert!(
        entries.is_empty(),
        "grit worktree diff must be empty, got {} entries",
        entries.len()
    );
}

fn assert_grit_clean(root: &Path) {
    let repo = open_worktree(root);
    let model = repo
        .status()
        .run(
            &StatusOptions {
                untracked: UntrackedMode::No,
                ..StatusOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    assert!(
        model.unstaged.is_empty() && model.untracked.is_empty(),
        "grit status must be clean, unstaged={:?}",
        model.unstaged
    );
}

#[test]
fn clone_long_path_status_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_bare_source(tmp.path());
    let dest = tmp.path().join("cg");
    let outcome = clone_repo(
        &bare,
        &dest,
        &CloneOptions::default(),
        &Environment::from_process_env(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("clone");
    assert_eq!(outcome.files, 2);
    let long = long_leaf_path(&dest);
    assert!(
        std::fs::symlink_metadata(&long).is_ok(),
        "long path file must exist after clone: {}",
        long.display()
    );
    assert_grit_clean(&dest);
    assert_grit_worktree_diff_empty(&dest);
    let stat = git_longpaths(&dest, &["diff", "--stat"]);
    assert!(
        stat.trim().is_empty(),
        "git diff --stat must be empty, got: {stat}"
    );
    let porcelain = git_longpaths(&dest, &["status", "-s"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status -s must be clean, got: {porcelain}"
    );
}

#[test]
fn real_deletion_of_short_file_still_reports() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = build_bare_source(tmp.path());
    let dest = tmp.path().join("cg");
    clone_repo(
        &bare,
        &dest,
        &CloneOptions::default(),
        &Environment::from_process_env(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("clone");
    std::fs::remove_file(dest.join("ok.txt")).expect("delete ok");
    let repo = open_worktree(&dest);
    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    assert!(
        model.unstaged.iter().any(|e| e.path == "ok.txt"),
        "ok.txt deletion must appear in status: {:?}",
        model.unstaged
    );
}

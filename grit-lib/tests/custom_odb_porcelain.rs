//! Porcelain staging, commit and fsck on a repository whose primary object store is custom.
//!
//! The index, refs and work tree live on disk, while every object goes through a
//! [`MemoryStore`] primary. Nothing may be written under `objects/`.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::porcelain::add::{stage, stage_for_commit, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

fn memory_repo() -> (tempfile::TempDir, Repository, Arc<MemoryStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(&root))
        .expect("open");
    (dir, repo, store)
}

fn work_tree(repo: &Repository) -> &Path {
    repo.work_tree().expect("work tree")
}

fn files_under(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in read.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
            }
        }
    }
    out.sort();
    out
}

fn blob_id(data: &[u8]) -> ObjectId {
    grit_lib::objects::hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, data).expect("hash")
}

fn commit_request(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        author: IDENT.to_owned(),
        committer: IDENT.to_owned(),
        ..Default::default()
    }
}

fn index_oid(repo: &Repository, path: &str) -> ObjectId {
    repo.index_handle()
        .load()
        .expect("index")
        .get(path.as_bytes(), 0)
        .unwrap_or_else(|| panic!("{path} staged"))
        .oid
}

/// Copy every object of `store` into the on-disk `objects/` of the repo so the system
/// `git` can verify what grit wrote through the custom primary.
fn export_store_and_git_fsck(store: &MemoryStore, root: &Path) {
    let disk = Odb::new(&root.join(".git/objects"));
    let mut oids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("enumerate");
    for oid in oids {
        let obj = store.read(&oid).expect("read").expect("present");
        assert_eq!(disk.write(obj.kind, &obj.data).expect("export"), oid);
    }
    let out = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--full", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git fsck");
    assert!(
        out.status.success(),
        "git fsck: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn stage_untracked_path_writes_blob_to_custom_primary() {
    let (_dir, repo, store) = memory_repo();
    let objects_before = files_under(repo.odb().objects_dir());
    fs::write(work_tree(&repo).join("a.txt"), "hello\n").expect("write");

    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["a.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect("stage untracked path");

    assert_eq!(outcome.added, vec!["a.txt".to_owned()]);
    let oid = blob_id(b"hello\n");
    assert_eq!(index_oid(&repo, "a.txt"), oid);
    assert!(store.contains(&oid).expect("contains"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
}

#[test]
fn stage_defaults_stage_untracked_paths_into_custom_primary() {
    let (_dir, repo, store) = memory_repo();
    let objects_before = files_under(repo.odb().objects_dir());
    fs::write(work_tree(&repo).join("one.txt"), "one\n").expect("write");
    fs::create_dir(work_tree(&repo).join("dir")).expect("mkdir");
    fs::write(work_tree(&repo).join("dir/two.txt"), "two\n").expect("write");

    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    assert_eq!(outcome.added.len(), 2, "{outcome:?}");
    assert!(store.contains(&blob_id(b"one\n")).expect("contains"));
    assert!(store.contains(&blob_id(b"two\n")).expect("contains"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
}

#[test]
fn stage_many_untracked_paths_uses_parallel_batch_on_custom_primary() {
    let (_dir, repo, store) = memory_repo();
    let objects_before = files_under(repo.odb().objects_dir());
    let count = 100;
    for i in 0..count {
        fs::write(
            work_tree(&repo).join(format!("f{i:03}.txt")),
            format!("file {i}\n"),
        )
        .expect("write");
    }

    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage_for_commit");

    assert_eq!(outcome.added.len(), count);
    for i in 0..count {
        let oid = blob_id(format!("file {i}\n").as_bytes());
        assert!(store.contains(&oid).expect("contains"), "f{i:03}.txt");
        assert_eq!(index_oid(&repo, &format!("f{i:03}.txt")), oid);
    }
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
}

#[test]
fn commit_worktree_with_untracked_files_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());
    fs::write(work_tree(&repo).join("a.txt"), "hello\n").expect("write");

    let first =
        commit_worktree(&repo, &commit_request("first"), &mut NullProgress).expect("commit");
    assert!(store.contains(&first.oid).expect("commit in store"));
    assert!(store.contains(&blob_id(b"hello\n")).expect("blob in store"));

    fs::write(work_tree(&repo).join("a.txt"), "hello again\n").expect("modify");
    fs::write(work_tree(&repo).join("b.txt"), "new file\n").expect("write");
    let second =
        commit_worktree(&repo, &commit_request("second"), &mut NullProgress).expect("commit");
    assert_eq!(second.parent, Some(first.oid));
    assert!(store
        .contains(&blob_id(b"hello again\n"))
        .expect("contains"));
    assert!(store.contains(&blob_id(b"new file\n")).expect("contains"));

    assert_eq!(
        files_under(repo.odb().objects_dir()),
        objects_before,
        "custom primary must not write loose objects"
    );
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn fsck_is_clean_on_healthy_custom_primary_repo() {
    let (_dir, repo, _store) = memory_repo();
    fs::write(work_tree(&repo).join("a.txt"), "hello\n").expect("write");
    fs::write(work_tree(&repo).join(".gitmodules"), "").expect("write");
    commit_worktree(&repo, &commit_request("first"), &mut NullProgress).expect("commit");

    let options = FsckOptions::full_repository(true);
    let report = repo
        .fsck(&options, &mut NullProgress, &NeverCancel)
        .expect("fsck on custom primary");
    let config = repo.config().expect("config");
    assert!(report.ok(&config, &options), "{:?}", report.issues);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert_eq!(report.objects_checked, 4, "two blobs, a tree and a commit");
    assert!(report.dangling.is_empty(), "{:?}", report.dangling);
}

#[test]
fn fsck_reports_bad_object_in_custom_primary() {
    let (_dir, repo, _store) = memory_repo();
    let bad_tree = repo
        .odb()
        .write(ObjectKind::Tree, b"100644 a\0short")
        .expect("write malformed tree");

    let options = FsckOptions::full_repository(false);
    let report = repo
        .fsck(&options, &mut NullProgress, &NeverCancel)
        .expect("fsck on custom primary");
    assert_eq!(report.objects_checked, 1);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.oid() == Some(bad_tree)),
        "{:?}",
        report.issues
    );
    assert!(!report.ok(&repo.config().expect("config"), &options));
}

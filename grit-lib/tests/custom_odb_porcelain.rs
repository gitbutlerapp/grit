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
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{MemoryRefStore, RefName};
use grit_lib::repo::{InitOptions, Repository};

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
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::store::{MemoryRefStore, RefName};
use grit_lib::repo::{InitOptions, Repository};

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
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .with_work_tree(&root)),
    )
    .expect("open");
    let opts = grit_lib::config::ConfigWriteOptions::new();
    repo.config()
        .set(
            grit_lib::config::ConfigScope::Local,
            "user.name",
            "A U Thor",
            &opts,
        )
        .expect("user.name");
    repo.config()
        .set(
            grit_lib::config::ConfigScope::Local,
            "user.email",
            "author@example.com",
            &opts,
        )
        .expect("user.email");
    repo.config()
        .set(
            grit_lib::config::ConfigScope::Local,
            "grit.changeIds",
            "false",
            &opts,
        )
        .expect("changeIds");
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
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(IDENT.parse().expect("signature"));
        req.committer = Some(IDENT.parse().expect("signature"));
        req
    }
}

fn head_oid(repo: &Repository, branch: &str) -> ObjectId {
    let refname = format!("refs/heads/{branch}");
    repo.references()
        .resolve(&RefName::try_from(refname.as_str()).expect("ref"))
        .expect("resolve")
}

fn switch(repo: &Repository, mode: SwitchMode) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: Default::default(),
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch");
}

fn commit_path(repo: &Repository, path: &str, contents: &str, message: &str) {
    fs::write(work_tree(repo).join(path), contents).expect("write");
    stage(
        repo,
        &StageOptions {
            pathspecs: vec![path.into()],
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    commit_worktree(
        repo,
        &commit_request(message),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
}

fn assert_merge_commit_in_store(repo: &Repository, store: &MemoryStore, merge_oid: ObjectId) {
    let commit = store
        .read(&merge_oid)
        .expect("read merge")
        .expect("merge present");
    assert_eq!(commit.kind, ObjectKind::Commit);
    let tree_oid = repo.odb().commit_tree(&merge_oid).expect("tree");
    assert!(
        store.contains(&tree_oid).expect("tree contains"),
        "merge tree must live in custom primary"
    );
    let tree = store
        .read(&tree_oid)
        .expect("read tree")
        .expect("tree present");
    assert_eq!(tree.kind, ObjectKind::Tree);
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
        &NeverCancel,
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

    let outcome = stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

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

    let outcome =
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage_for_commit");

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

    let first = commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    assert!(store.contains(&first.oid).expect("commit in store"));
    assert!(store.contains(&blob_id(b"hello\n")).expect("blob in store"));

    fs::write(work_tree(&repo).join("a.txt"), "hello again\n").expect("modify");
    fs::write(work_tree(&repo).join("b.txt"), "new file\n").expect("write");
    let second = commit_worktree(
        &repo,
        &commit_request("second"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
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

/// With both stores injected the git directory may be missing: staging creates it (the
/// index lock creates its parent), and commit then records its files there.
#[test]
fn stage_and_commit_create_a_missing_git_dir_when_every_store_is_injected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    let git_dir = root.join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_initial_branch("main")
            .with_work_tree(&root)),
    )
    .expect("open");
    assert!(!git_dir.exists());

    fs::write(root.join("a.txt"), "hello\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["a.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    assert!(git_dir.join("index").is_file());

    let commit = commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    assert!(store.contains(&commit.oid).expect("commit in store"));
    assert_eq!(files_under(&git_dir), ["COMMIT_EDITMSG", "index"]);
}

#[test]
fn fsck_is_clean_on_healthy_custom_primary_repo() {
    let (_dir, repo, _store) = memory_repo();
    fs::write(work_tree(&repo).join("a.txt"), "hello\n").expect("write");
    fs::write(work_tree(&repo).join(".gitmodules"), "").expect("write");
    commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

    let options = FsckOptions::full_repository(true);
    let report = repo
        .fsck(&options, &mut NullProgress, &NeverCancel)
        .expect("fsck on custom primary");
    let config = repo.config().snapshot().expect("config");
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
    assert!(!report.ok(&repo.config().snapshot().expect("config"), &options));
}

#[test]
fn integrate_branch_clean_merge_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "shared.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "shared.txt", "topic\n", "topic");
    let topic = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );
    commit_path(&repo, "main-only.txt", "main\n", "main");
    let main = head_oid(&repo, "main");

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected clean merge, got {outcome:?}");
    };
    assert_eq!(head_oid(&repo, "main"), oid);
    assert_merge_commit_in_store(&repo, &store, oid);
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn start_replay_sequence_pick_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "a.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "b.txt", "topic\n", "topic");
    let pick = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );

    let req = ReplaySequenceRequest {
        commits: vec![pick],
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("committer"),
        options: ReplayOptions::default(),
    };
    let out = start_replay_sequence(&repo, &req, &mut NullProgress, &NeverCancel).expect("pick");
    let SequenceOutcome::Completed { created, .. } = out else {
        panic!("expected completed pick, got {out:?}");
    };
    assert_eq!(created.len(), 1);
    let new_oid = created[0].new_oid;
    assert_eq!(head_oid(&repo, "main"), new_oid);
    assert!(store.contains(&new_oid).expect("pick commit in store"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn start_replay_sequence_revert_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "a.txt", "base\n", "base");
    commit_path(&repo, "a.txt", "second\n", "second");
    let revert_target = head_oid(&repo, "main");

    let req = ReplaySequenceRequest {
        commits: vec![revert_target],
        direction: ReplayDirection::Revert,
        committer: IDENT.parse().expect("committer"),
        options: ReplayOptions::default(),
    };
    let out = start_replay_sequence(&repo, &req, &mut NullProgress, &NeverCancel).expect("revert");
    let SequenceOutcome::Completed { created, .. } = out else {
        panic!("expected completed revert, got {out:?}");
    };
    assert_eq!(created.len(), 1);
    let new_oid = created[0].new_oid;
    assert_eq!(head_oid(&repo, "main"), new_oid);
    assert!(store.contains(&new_oid).expect("revert commit in store"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn integrate_branch_mixed_conflict_persists_clean_blob_on_custom_primary() {
    let (_dir, repo, store) = memory_repo();
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "shared.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "shared.txt", "topic edit\n", "topic shared");
    fs::write(work_tree(&repo).join("topic-only.txt"), "only topic\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["shared.txt".into(), "topic-only.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    commit_worktree(
        &repo,
        &commit_request("topic"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    let topic = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );
    commit_path(&repo, "shared.txt", "main edit\n", "main shared");
    let main = head_oid(&repo, "main");

    let topic_only_oid = blob_id(b"only topic\n");
    assert!(store.contains(&topic_only_oid).expect("topic blob"));

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert!(paths.iter().any(|p| p == "shared.txt"));
    assert!(
        store.contains(&topic_only_oid).expect("contains"),
        "clean auto-merged blob must be persisted from overlay"
    );
    assert_eq!(head_oid(&repo, "main"), main, "branch must not move");
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
}
        .expect("user.name");
    repo.config()
        .set(
            grit_lib::config::ConfigScope::Local,
            "user.email",
            "author@example.com",
            &opts,
        )
        .expect("user.email");
    repo.config()
        .set(
            grit_lib::config::ConfigScope::Local,
            "grit.changeIds",
            "false",
            &opts,
        )
        .expect("changeIds");
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
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(IDENT.parse().expect("signature"));
        req.committer = Some(IDENT.parse().expect("signature"));
        req
    }
}

fn head_oid(repo: &Repository, branch: &str) -> ObjectId {
    let refname = format!("refs/heads/{branch}");
    repo.references()
        .resolve(&RefName::try_from(refname.as_str()).expect("ref"))
        .expect("resolve")
}

fn switch(repo: &Repository, mode: SwitchMode) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: Default::default(),
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("switch");
}

fn commit_path(repo: &Repository, path: &str, contents: &str, message: &str) {
    fs::write(work_tree(repo).join(path), contents).expect("write");
    stage(
        repo,
        &StageOptions {
            pathspecs: vec![path.into()],
            ..Default::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");
    commit_worktree(
        repo,
        &commit_request(message),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
}

fn assert_merge_commit_in_store(repo: &Repository, store: &MemoryStore, merge_oid: ObjectId) {
    let commit = store
        .read(&merge_oid)
        .expect("read merge")
        .expect("merge present");
    assert_eq!(commit.kind, ObjectKind::Commit);
    let tree_oid = repo.odb().commit_tree(&merge_oid).expect("tree");
    assert!(
        store.contains(&tree_oid).expect("tree contains"),
        "merge tree must live in custom primary"
    );
    let tree = store
        .read(&tree_oid)
        .expect("read tree")
        .expect("tree present");
    assert_eq!(tree.kind, ObjectKind::Tree);
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
        &CancelToken::never(),
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

    let outcome = stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");

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

    let outcome = stage_for_commit(&repo, &mut NullProgress, &CancelToken::never())
        .expect("stage_for_commit");

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

    let first = commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
    assert!(store.contains(&first.oid).expect("commit in store"));
    assert!(store.contains(&blob_id(b"hello\n")).expect("blob in store"));

    fs::write(work_tree(&repo).join("a.txt"), "hello again\n").expect("modify");
    fs::write(work_tree(&repo).join("b.txt"), "new file\n").expect("write");
    let second = commit_worktree(
        &repo,
        &commit_request("second"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
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

/// With both stores injected the git directory may be missing: staging creates it (the
/// index lock creates its parent), and commit then records its files there.
#[test]
fn stage_and_commit_create_a_missing_git_dir_when_every_store_is_injected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    let git_dir = root.join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_initial_branch("main")
            .with_work_tree(&root)),
    )
    .expect("open");
    assert!(!git_dir.exists());

    fs::write(root.join("a.txt"), "hello\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["a.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");
    assert!(git_dir.join("index").is_file());

    let commit = commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
    assert!(store.contains(&commit.oid).expect("commit in store"));
    assert_eq!(files_under(&git_dir), ["COMMIT_EDITMSG", "index"]);
}

#[test]
fn fsck_is_clean_on_healthy_custom_primary_repo() {
    let (_dir, repo, _store) = memory_repo();
    fs::write(work_tree(&repo).join("a.txt"), "hello\n").expect("write");
    fs::write(work_tree(&repo).join(".gitmodules"), "").expect("write");
    commit_worktree(
        &repo,
        &commit_request("first"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");

    let options = FsckOptions::full_repository(true);
    let report = repo
        .fsck(&options, &mut NullProgress, &CancelToken::never())
        .expect("fsck on custom primary");
    let config = repo.config().snapshot().expect("config");
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
        .fsck(&options, &mut NullProgress, &CancelToken::never())
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
    assert!(!report.ok(&repo.config().snapshot().expect("config"), &options));
}

#[test]
fn integrate_branch_clean_merge_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "shared.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "shared.txt", "topic\n", "topic");
    let topic = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );
    commit_path(&repo, "main-only.txt", "main\n", "main");
    let main = head_oid(&repo, "main");

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected clean merge, got {outcome:?}");
    };
    assert_eq!(head_oid(&repo, "main"), oid);
    assert_merge_commit_in_store(&repo, &store, oid);
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn start_replay_sequence_pick_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "a.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "b.txt", "topic\n", "topic");
    let pick = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );

    let req = ReplaySequenceRequest {
        commits: vec![pick],
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("committer"),
        options: ReplayOptions::default(),
    };
    let out =
        start_replay_sequence(&repo, &req, &mut NullProgress, &CancelToken::never()).expect("pick");
    let SequenceOutcome::Completed { created, .. } = out else {
        panic!("expected completed pick, got {out:?}");
    };
    assert_eq!(created.len(), 1);
    let new_oid = created[0].new_oid;
    assert_eq!(head_oid(&repo, "main"), new_oid);
    assert!(store.contains(&new_oid).expect("pick commit in store"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn start_replay_sequence_revert_on_custom_primary() {
    let (dir, repo, store) = memory_repo();
    let root = dir.path().canonicalize().expect("root");
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "a.txt", "base\n", "base");
    commit_path(&repo, "a.txt", "second\n", "second");
    let revert_target = head_oid(&repo, "main");

    let req = ReplaySequenceRequest {
        commits: vec![revert_target],
        direction: ReplayDirection::Revert,
        committer: IDENT.parse().expect("committer"),
        options: ReplayOptions::default(),
    };
    let out = start_replay_sequence(&repo, &req, &mut NullProgress, &CancelToken::never())
        .expect("revert");
    let SequenceOutcome::Completed { created, .. } = out else {
        panic!("expected completed revert, got {out:?}");
    };
    assert_eq!(created.len(), 1);
    let new_oid = created[0].new_oid;
    assert_eq!(head_oid(&repo, "main"), new_oid);
    assert!(store.contains(&new_oid).expect("revert commit in store"));
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
    export_store_and_git_fsck(&store, &root);
}

#[test]
fn integrate_branch_mixed_conflict_persists_clean_blob_on_custom_primary() {
    let (_dir, repo, store) = memory_repo();
    let objects_before = files_under(repo.odb().objects_dir());

    commit_path(&repo, "shared.txt", "base\n", "base");
    let base = head_oid(&repo, "main");
    switch(
        &repo,
        SwitchMode::Create {
            branch: "topic".into(),
            start_oid: base,
            create_source_label: "HEAD".into(),
            track_upstream: None,
        },
    );
    commit_path(&repo, "shared.txt", "topic edit\n", "topic shared");
    fs::write(work_tree(&repo).join("topic-only.txt"), "only topic\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["shared.txt".into(), "topic-only.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");
    commit_worktree(
        &repo,
        &commit_request("topic"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
    let topic = head_oid(&repo, "topic");
    switch(
        &repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: base,
        },
    );
    commit_path(&repo, "shared.txt", "main edit\n", "main shared");
    let main = head_oid(&repo, "main");

    let topic_only_oid = blob_id(b"only topic\n");
    assert!(store.contains(&topic_only_oid).expect("topic blob"));

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert!(paths.iter().any(|p| p == "shared.txt"));
    assert!(
        store.contains(&topic_only_oid).expect("contains"),
        "clean auto-merged blob must be persisted from overlay"
    );
    assert_eq!(head_oid(&repo, "main"), main, "branch must not move");
    assert_eq!(files_under(repo.odb().objects_dir()), objects_before);
}

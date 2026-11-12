//! A pre-cancelled [`CancelToken`] stops each long-running operation that takes the trailing
//! `progress, cancel` pair with [`Error::Cancelled`], and leaves the repository as it was.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use grit_lib::diff::TreeDiffOptions;
use grit_lib::error::Error;
use grit_lib::maintenance::{
    CommitGraphOptions, DropPromisedBlobsOptions, MidxOptions, OffloadOptions, PruneOptions,
};
use grit_lib::objects::index_pack::{ingest_received_pack, IngestPackOptions};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::pack_objects::{
    build_pack, PackBuildOptions, PackObjects, PackObjectsOptions, UploadPackStream,
};
use grit_lib::objects::unpack_objects::{ingest_pack_into, UnpackOptions};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::commit::CommitScope;
use grit_lib::porcelain::paths::{MoveOptions, RemoveOptions};
use grit_lib::porcelain::reset::{ResetMode, ResetOptions};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::{AddLinkedWorktreeOptions, MoveWorktreeOptions, WorktreeBranch};

const IDENT: &str = "T <t@t> 0 +0000";

fn cancelled() -> CancelToken {
    let token = CancelToken::new();
    token.cancel();
    token
}

fn assert_cancelled<T: std::fmt::Debug>(result: grit_lib::error::Result<T>) {
    match result {
        Err(Error::Cancelled) => {}
        other => panic!("expected Error::Cancelled, got {other:?}"),
    }
}

/// A repository with one commit of a few files on `main`.
fn repo_with_commit() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    for i in 0..4 {
        fs::write(tmp.path().join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(
                grit_test_support::commit_request("one", IDENT),
                CommitScope::AutoStage,
            ),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit");
    (tmp, repo)
}

fn head(repo: &Repository) -> ObjectId {
    repo.checkout()
        .resolve_revision_spec("HEAD")
        .expect("resolve HEAD")
}

fn loose_object_count(git_dir: &Path) -> usize {
    let objects = git_dir.join("objects");
    fs::read_dir(&objects)
        .expect("objects dir")
        .flatten()
        .filter(|e| e.file_name().len() == 2)
        .map(|e| fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
        .sum()
}

fn pack_count(git_dir: &Path) -> usize {
    fs::read_dir(git_dir.join("objects").join("pack"))
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn maintenance_steps_stop_on_a_cancelled_token() {
    let (_tmp, repo) = repo_with_commit();
    let git_dir = repo.git_dir().to_path_buf();
    let loose = loose_object_count(&git_dir);
    let cancel = cancelled();
    let maintenance = repo.maintenance();

    assert_cancelled(maintenance.prune_loose_objects(
        &PruneOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert_cancelled(maintenance.build_commit_graph(
        &CommitGraphOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert_cancelled(maintenance.write_commit_graph(
        &CommitGraphOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert_cancelled(maintenance.write_midx(&MidxOptions::default(), &mut NullProgress, &cancel));
    assert_cancelled(maintenance.offload_blobs(
        &OffloadOptions::new("lop", 0),
        &mut NullProgress,
        &cancel,
    ));
    assert_cancelled(maintenance.drop_promised_blobs(
        &DropPromisedBlobsOptions::new(time::OffsetDateTime::UNIX_EPOCH, 1, None),
        &mut NullProgress,
        &cancel,
    ));

    assert_eq!(loose_object_count(&git_dir), loose);
    assert!(!git_dir.join("objects/info/commit-graph").exists());
    assert!(!git_dir.join("objects/pack/multi-pack-index").exists());

    // The same calls run to completion with a live token.
    let report = maintenance
        .write_commit_graph(
            &CommitGraphOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("write commit-graph");
    assert_eq!(report.commit_count, 1);
}

#[test]
fn pack_ingest_and_pack_writing_stop_on_a_cancelled_token() {
    let (_tmp, repo) = repo_with_commit();
    let tip = head(&repo);
    let pack = build_pack(repo.odb(), &[tip], &[], &PackBuildOptions::default()).expect("pack");
    let cancel = cancelled();

    let (_target_dir, target) = {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (target, _) = Repository::init(tmp.path(), &InitOptions::new()).expect("init target");
        (tmp, target)
    };
    let packs_before = pack_count(target.git_dir());
    assert_cancelled(ingest_received_pack(
        pack.clone(),
        target.odb(),
        &IngestPackOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert_eq!(pack_count(target.git_dir()), packs_before);
    assert!(!target.odb().contains(&tip).expect("contains"));

    let store = MemoryStore::new(HashAlgo::Sha1);
    assert_cancelled(ingest_pack_into(
        &mut Cursor::new(pack),
        &store,
        &store,
        &UnpackOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert!(!store.contains(&tip).expect("contains"));

    let mut out = Vec::new();
    assert_cancelled(
        PackObjects::new(repo.odb(), PackObjectsOptions::default())
            .wants(&[tip])
            .write_to(&mut out, &mut NullProgress, &cancel),
    );

    let wants = [tip];
    let mut out = Vec::new();
    assert_cancelled(
        UploadPackStream::new(&wants, PackBuildOptions::default()).write(
            &repo,
            &mut out,
            &mut NullProgress,
            &cancel,
        ),
    );
}

#[test]
fn work_tree_diffs_stop_on_a_cancelled_token() {
    let (tmp, repo) = repo_with_commit();
    fs::write(tmp.path().join("f0.txt"), "changed\n").expect("write");
    let cancel = cancelled();
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let tree = repo.head_tree().expect("head tree");

    assert_cancelled(repo.diff().index_to_worktree(
        &mut index,
        &TreeDiffOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert_cancelled(repo.diff().tree_to_worktree(
        tree.as_ref(),
        &index,
        &TreeDiffOptions::default(),
        &mut NullProgress,
        &cancel,
    ));

    let diff = repo
        .diff()
        .index_to_worktree(
            &mut index,
            &TreeDiffOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("diff");
    assert_eq!(diff.entries.len(), 1);
}

#[test]
fn linked_worktree_operations_stop_on_a_cancelled_token() {
    let (tmp, repo) = repo_with_commit();
    let cancel = cancelled();
    let wt = tmp.path().join("wt");

    assert_cancelled(
        repo.worktrees().add(
            &AddLinkedWorktreeOptions::new(wt.clone())
                .with_branch(Some(WorktreeBranch::Create("feature".to_owned()))),
            &mut NullProgress,
            &cancel,
        ),
    );
    assert!(!wt.exists());
    assert!(!repo.common_git_dir().join("worktrees").join("wt").exists());
    assert!(repo
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/heads/feature").expect("valid ref name")
        )
        .is_err());

    repo.worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(wt.clone())
                .with_branch(Some(WorktreeBranch::Create("feature".to_owned()))),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("add worktree");

    let moved = tmp.path().join("moved");
    assert_cancelled(repo.worktrees().move_linked(
        &wt,
        &moved,
        &MoveWorktreeOptions::default(),
        &mut NullProgress,
        &cancel,
    ));
    assert!(wt.join(".git").exists());
    assert!(!moved.exists());

    assert_cancelled(
        repo.worktrees()
            .remove(&wt, true, &mut NullProgress, &cancel),
    );
    assert!(wt.join(".git").exists());
    assert_eq!(repo.worktrees().list().expect("list").len(), 2);
}

#[test]
fn checkout_path_operations_stop_on_a_cancelled_token() {
    let (tmp, repo) = repo_with_commit();
    let cancel = cancelled();
    let tip = head(&repo);

    assert_cancelled(
        repo.checkout().remove_paths(
            &RemoveOptions::default()
                .with_pathspecs(vec!["f1.txt".into()])
                .with_pathspec_sources(vec!["f1.txt".into()]),
            &mut NullProgress,
            &cancel,
        ),
    );
    assert!(tmp.path().join("f1.txt").exists());

    assert_cancelled(repo.checkout().move_paths(
        &MoveOptions::new(vec!["f2.txt".to_owned()], "g2.txt".to_owned()),
        &mut NullProgress,
        &cancel,
    ));
    assert!(tmp.path().join("f2.txt").exists());
    assert!(!tmp.path().join("g2.txt").exists());

    assert_cancelled(repo.checkout().preview_reset(
        &ResetOptions::new(
            tip,
            ResetMode::Hard,
            IDENT.parse().expect("IDENT"),
            "reset probe",
        ),
        &mut NullProgress,
        &cancel,
    ));
    assert_eq!(head(&repo), tip);
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    assert_eq!(index.entries().len(), 4);
}

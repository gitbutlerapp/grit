//! Case-insensitive checkout collisions: warn and auto-stage safety for loser paths.

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{entry_from_stat, Index, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;

fn enable_ignorecase(repo: &Repository, root: &Path) {
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tignorecase = true\n",
    )
    .expect("config");
    repo.reload_config().expect("reload");
}

fn build_collision_tree(repo: &Repository) -> grit_lib::objects::ObjectId {
    let wt = repo.work_tree().expect("worktree");
    let mut index = Index::new();
    for name in ["File.txt", "file.txt", "dir/A.txt", "Dir/a.txt"] {
        let path = wt.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, format!("{name}\n")).expect("write");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, format!("{name}\n").as_bytes())
            .expect("blob");
        index.add_or_replace(
            entry_from_stat(&path, name.as_bytes(), oid, MODE_REGULAR).expect("entry"),
        );
    }
    index.sort();
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent()).expect("tree")
}

#[test]
fn populated_checkout_reports_collisions_and_auto_stage_skips_losers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("grit-wt");
    fs::create_dir(&dest).expect("mkdir");
    let repo = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("master".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    enable_ignorecase(&repo, &dest);

    let tree = build_collision_tree(&repo);
    // Reset worktree before populated checkout (tree built in place).
    for name in ["File.txt", "file.txt", "dir/A.txt", "Dir/a.txt"] {
        let _ = fs::remove_file(dest.join(name));
    }

    let checkout =
        checkout_between_trees_with_progress(&repo, None, &tree, &mut NullProgress, &NeverCancel)
            .expect("checkout");

    assert_eq!(
        checkout.path_collisions,
        vec![
            "Dir/a.txt".to_owned(),
            "File.txt".to_owned(),
            "dir/A.txt".to_owned(),
            "file.txt".to_owned(),
        ]
    );

    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"File.txt", 0).is_some(),
        "checkout must keep File.txt in index"
    );
    let file_txt_oid = index.get(b"File.txt", 0).expect("File.txt").oid;
    let dir_a_oid = index.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid;
    // On a case-sensitive host both spellings may exist; keep one file per collision group.
    let _ = fs::remove_file(dest.join("File.txt"));
    let _ = fs::remove_file(dest.join("Dir/a.txt"));

    fs::write(dest.join("unrelated.txt"), "edit\n").expect("unrelated");

    stage_tracked_modifications(&repo, &dest).expect("stage tracked");
    let mid = repo.index_handle().load().expect("mid index");
    assert!(
        mid.get(b"File.txt", 0).is_some(),
        "auto-stage scan must keep collision loser File.txt in the index"
    );
    assert_eq!(
        mid.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid,
        dir_a_oid,
        "stage tracked must not rewrite collision loser Dir/a.txt"
    );

    let id = "Tester <t@example.com> 1700000000 +0000".to_owned();
    let req = CommitRequest {
        message: "unrelated change".to_owned(),
        author: Some(id.parse().expect("sig")),
        committer: Some(id.parse().expect("sig")),
        allow_empty: false,
        ..Default::default()
    };
    commit_with_scope(&repo, &req, CommitScope::AutoStage, &mut NullProgress).expect("commit");

    let post = repo.index_handle().load().expect("index after");
    assert_eq!(
        post.get(b"File.txt", 0).expect("File.txt").oid,
        file_txt_oid
    );
    assert_eq!(post.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid, dir_a_oid);
}

struct TestRepo {
    _tmp: tempfile::TempDir,
    repo: Repository,
    dest: PathBuf,
}

fn init_collision_repo() -> TestRepo {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("grit-wt");
    fs::create_dir(&dest).expect("mkdir");
    let repo = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("master".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    enable_ignorecase(&repo, &dest);
    TestRepo {
        _tmp: tmp,
        repo,
        dest,
    }
}

fn checkout_collision_tree(repo: &Repository, dest: &Path) {
    let tree = build_collision_tree(repo);
    for name in ["File.txt", "file.txt", "dir/A.txt", "Dir/a.txt"] {
        let _ = fs::remove_file(dest.join(name));
    }
    checkout_between_trees_with_progress(repo, None, &tree, &mut NullProgress, &NeverCancel)
        .expect("checkout");
}

fn keep_one_spelling_per_collision_group(dest: &Path) {
    let _ = fs::remove_file(dest.join("File.txt"));
    let _ = fs::remove_file(dest.join("Dir/a.txt"));
}

fn auto_stage_unrelated_and_commit(repo: &Repository, dest: &Path) {
    fs::write(dest.join("unrelated.txt"), "edit\n").expect("unrelated");
    stage_tracked_modifications(repo, dest).expect("stage tracked");
    let id = "Tester <t@example.com> 1700000000 +0000".to_owned();
    let req = CommitRequest {
        message: "unrelated change".to_owned(),
        author: Some(id.parse().expect("sig")),
        committer: Some(id.parse().expect("sig")),
        allow_empty: false,
        ..Default::default()
    };
    commit_with_scope(repo, &req, CommitScope::AutoStage, &mut NullProgress).expect("commit");
}

#[test]
fn auto_stage_updates_edited_winner_and_preserves_loser_oids() {
    let fx = init_collision_repo();
    checkout_collision_tree(&fx.repo, &fx.dest);
    keep_one_spelling_per_collision_group(&fx.dest);

    let index = fx.repo.index_handle().load().expect("index");
    let file_loser_oid = index.get(b"File.txt", 0).expect("File.txt").oid;
    let dir_loser_oid = index.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid;
    let winner_before = index.get(b"file.txt", 0).expect("file.txt").oid;

    fs::write(fx.dest.join("file.txt"), b"edited winner\n").expect("edit winner");
    auto_stage_unrelated_and_commit(&fx.repo, &fx.dest);

    let post = fx.repo.index_handle().load().expect("index after");
    assert_ne!(
        post.get(b"file.txt", 0).expect("file.txt").oid,
        winner_before,
        "dentry winner must pick up edited worktree bytes"
    );
    assert_eq!(
        post.get(b"File.txt", 0).expect("File.txt").oid,
        file_loser_oid
    );
    assert_eq!(
        post.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid,
        dir_loser_oid
    );
}

#[test]
fn deleting_dentry_winner_stages_removal_and_keeps_losers() {
    let fx = init_collision_repo();
    checkout_collision_tree(&fx.repo, &fx.dest);
    keep_one_spelling_per_collision_group(&fx.dest);

    let index = fx.repo.index_handle().load().expect("index");
    let file_loser_oid = index.get(b"File.txt", 0).expect("File.txt").oid;
    let dir_loser_oid = index.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid;

    fs::remove_file(fx.dest.join("file.txt")).expect("delete winner");
    fs::remove_file(fx.dest.join("dir/A.txt")).expect("delete nested winner");
    stage_tracked_modifications(&fx.repo, &fx.dest).expect("stage tracked");

    let mid = fx.repo.index_handle().load().expect("mid index");
    assert!(
        mid.get(b"file.txt", 0).is_none(),
        "winner removed from index"
    );
    assert_eq!(
        mid.get(b"File.txt", 0).expect("File.txt").oid,
        file_loser_oid
    );
    assert!(mid.get(b"dir/A.txt", 0).is_none(), "nested winner removed");
    assert_eq!(
        mid.get(b"Dir/a.txt", 0).expect("Dir/a.txt").oid,
        dir_loser_oid
    );
}

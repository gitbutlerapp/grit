//! Case-insensitive checkout collisions: warn and auto-stage safety for loser paths.

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::error::Error;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{entry_from_stat, Index, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::pathspec::ResolvedPathspecs;
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
    let req = {
        let mut o = CommitRequest::default();
        o.message = "unrelated change".to_owned();
        o.author = Some(id.parse().expect("sig"));
        o.committer = Some(id.parse().expect("sig"));
        o.allow_empty = false;
        o
    };
    commit_with_scope(
        &repo,
        &req,
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

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
    let req = {
        let mut o = CommitRequest::default();
        o.message = "unrelated change".to_owned();
        o.author = Some(id.parse().expect("sig"));
        o.committer = Some(id.parse().expect("sig"));
        o.allow_empty = false;
        o
    };
    commit_with_scope(
        repo,
        &req,
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
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

fn collision_clone_worktree(fx: &TestRepo) {
    checkout_collision_tree(&fx.repo, &fx.dest);
    keep_one_spelling_per_collision_group(&fx.dest);
    let pre = fx.repo.index_handle().load().expect("index");
    assert!(
        pre.get(b"File.txt", 0).is_some() && pre.get(b"file.txt", 0).is_some(),
        "checkout index must keep both collision spellings"
    );
    commit_with_scope(
        &fx.repo,
        &commit_request("base"),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("seed HEAD from collision index");
}

fn commit_request(message: &str) -> CommitRequest {
    let id = "Tester <t@example.com> 1700000000 +0000".to_owned();
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("sig"));
        o.committer = Some(id.parse().expect("sig"));
        o.allow_empty = false;
        o
    }
}

fn head_oid(repo: &Repository) -> grit_lib::objects::ObjectId {
    grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").expect("head")
}

#[test]
fn collision_loser_path_commit_leaves_index_and_head_unchanged() {
    let fx = init_collision_repo();
    collision_clone_worktree(&fx);

    let before = fx.repo.index_handle().load().expect("index");
    assert!(before.get(b"file.txt", 0).is_some(), "pre: file.txt");
    let file_loser_oid = before.get(b"File.txt", 0).expect("File.txt").oid;
    let head_before = head_oid(&fx.repo);

    let err = commit_with_scope(
        &fx.repo,
        &commit_request("e"),
        CommitScope::Paths(ResolvedPathspecs {
            specs: vec!["File.txt".into()],
            sources: vec!["File.txt".into()],
            scoped: true,
        }),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("path commit must not delete collision loser");
    assert!(
        matches!(
            err,
            Error::NothingToCommit | Error::NothingStagedToCommit { .. }
        ),
        "{err:?}"
    );

    assert_eq!(head_oid(&fx.repo), head_before);
    let after = fx.repo.index_handle().load().expect("index after");
    assert_eq!(
        after.get(b"File.txt", 0).expect("File.txt").oid,
        file_loser_oid
    );
    assert!(after.get(b"file.txt", 0).is_some());
}

#[test]
fn collision_loser_auto_stage_commit_is_noop() {
    let fx = init_collision_repo();
    collision_clone_worktree(&fx);

    let before = fx.repo.index_handle().load().expect("index");
    let file_loser_oid = before.get(b"File.txt", 0).expect("File.txt").oid;
    let head_before = head_oid(&fx.repo);

    let err = commit_with_scope(
        &fx.repo,
        &commit_request("e"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("auto-stage must not commit phantom loser changes");
    assert!(
        matches!(err, Error::NothingStagedToCommit { .. }),
        "collision drift must not read as a clean worktree: {err:?}"
    );

    assert_eq!(head_oid(&fx.repo), head_before);
    let after = fx.repo.index_handle().load().expect("index after");
    assert_eq!(
        after.get(b"File.txt", 0).expect("File.txt").oid,
        file_loser_oid
    );
}

fn build_directory_name_collision_index(
    repo: &Repository,
    dest: &Path,
) -> (grit_lib::objects::ObjectId, grit_lib::objects::ObjectId) {
    let oid_one = repo.odb().write(ObjectKind::Blob, b"one\n").expect("one");
    let oid_two = repo.odb().write(ObjectKind::Blob, b"two\n").expect("two");
    fs::create_dir_all(dest.join("Docs")).expect("mkdir");
    fs::write(dest.join("Docs/guide.md"), b"two\n").expect("on-disk bytes match docs/");
    let mut index = Index::new();
    index.add_or_replace(
        entry_from_stat(
            &dest.join("Docs/guide.md"),
            b"Docs/guide.md",
            oid_one,
            MODE_REGULAR,
        )
        .expect("entry"),
    );
    index.add_or_replace(
        entry_from_stat(
            &dest.join("Docs/guide.md"),
            b"docs/guide.md",
            oid_two,
            MODE_REGULAR,
        )
        .expect("entry"),
    );
    index.sort();
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent()).expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    (oid_one, oid_two)
}

fn commit_directory_collision_head(
    fx: &TestRepo,
) -> (grit_lib::objects::ObjectId, grit_lib::objects::ObjectId) {
    let oids = build_directory_name_collision_index(&fx.repo, &fx.dest);
    commit_with_scope(
        &fx.repo,
        &commit_request("base"),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("seed HEAD");
    oids
}

#[test]
fn directory_collision_auto_stage_commit_preserves_both_index_oids() {
    let fx = init_collision_repo();
    let (docs_upper_oid, docs_lower_oid) = commit_directory_collision_head(&fx);

    fs::write(fx.dest.join("unrelated.txt"), "edit\n").expect("unrelated");
    stage_tracked_modifications(&fx.repo, &fx.dest).expect("stage tracked");

    let mid = fx.repo.index_handle().load().expect("mid index");
    assert_eq!(
        mid.get(b"Docs/guide.md", 0).expect("Docs/guide.md").oid,
        docs_upper_oid,
        "dentry winner must keep its index oid when worktree holds sibling bytes"
    );
    assert_eq!(
        mid.get(b"docs/guide.md", 0).expect("docs/guide.md").oid,
        docs_lower_oid
    );

    auto_stage_unrelated_and_commit(&fx.repo, &fx.dest);

    let post = fx.repo.index_handle().load().expect("post commit");
    assert_eq!(
        post.get(b"Docs/guide.md", 0).expect("Docs/guide.md").oid,
        docs_upper_oid
    );
    assert_eq!(
        post.get(b"docs/guide.md", 0).expect("docs/guide.md").oid,
        docs_lower_oid
    );
}

#[test]
fn directory_collision_winner_omitted_from_status_when_bytes_match_sibling() {
    let fx = init_collision_repo();
    commit_directory_collision_head(&fx);

    let model = status(
        &fx.repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.iter().any(|e| e.path() == "Docs/guide.md"),
        "index row whose oid differs from on-disk bytes must be unstaged: {:?}",
        model.unstaged.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

#[test]
fn directory_collision_edited_winner_updates_only_dentry_spelling() {
    let fx = init_collision_repo();
    let (docs_upper_oid, docs_lower_oid) = commit_directory_collision_head(&fx);

    fs::write(fx.dest.join("Docs/guide.md"), b"brand-new\n").expect("edit");
    stage_tracked_modifications(&fx.repo, &fx.dest).expect("stage");

    let mid = fx.repo.index_handle().load().expect("mid");
    assert_ne!(
        mid.get(b"Docs/guide.md", 0).expect("Docs/guide.md").oid,
        docs_upper_oid
    );
    assert_eq!(
        mid.get(b"docs/guide.md", 0).expect("docs/guide.md").oid,
        docs_lower_oid
    );
}

#[test]
fn collision_loser_omitted_from_status_unstaged() {
    let fx = init_collision_repo();
    collision_clone_worktree(&fx);
    let _ = fs::remove_file(fx.dest.join("file.txt"));
    let _ = fs::remove_file(fx.dest.join("dir/A.txt"));

    let model = status(
        &fx.repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        !model.unstaged.iter().any(|e| e.path() == "File.txt"),
        "missing collision loser must not appear as unstaged or deleted: {:?}",
        model.unstaged.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

fn build_flat_name_collision_tree(repo: &Repository, dest: &Path) -> grit_lib::objects::ObjectId {
    let oid_one = repo.odb().write(ObjectKind::Blob, b"one\n").expect("one");
    let oid_two = repo
        .odb()
        .write(ObjectKind::Blob, b"twotwotwo\n")
        .expect("two");
    fs::write(dest.join("a.txt"), b"twotwotwo\n").expect("on-disk bytes match a.txt");
    let mut index = Index::new();
    index.add_or_replace(
        entry_from_stat(&dest.join("a.txt"), b"A.txt", oid_one, MODE_REGULAR).expect("A"),
    );
    index.add_or_replace(
        entry_from_stat(&dest.join("a.txt"), b"a.txt", oid_two, MODE_REGULAR).expect("a"),
    );
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    tree
}

#[test]
fn switch_reports_collisions_and_status_lists_mismatched_index_row() {
    let fx = init_collision_repo();
    let empty = {
        let mut o = commit_request("empty");
        o.message = "empty".into();
        o.allow_empty = true;
        o
    };
    commit_with_scope(
        &fx.repo,
        &empty,
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("empty main");
    let main_tip = head_oid(&fx.repo);

    build_flat_name_collision_tree(&fx.repo, &fx.dest);
    commit_with_scope(
        &fx.repo,
        &commit_request("collide"),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("collide commit");
    let collide_tip = head_oid(&fx.repo);
    grit_lib::refs::write_ref(fx.repo.git_dir(), "refs/heads/collide", &collide_tip)
        .expect("collide ref");
    grit_lib::refs::write_ref(fx.repo.git_dir(), "refs/heads/master", &main_tip).expect("main");
    grit_lib::refs::write_symbolic_ref(fx.repo.git_dir(), "HEAD", "refs/heads/master")
        .expect("head main");
    let _ = fs::remove_file(fx.dest.join("A.txt"));
    let _ = fs::remove_file(fx.dest.join("a.txt"));
    fx.repo
        .index_handle()
        .write(&mut Index::new())
        .expect("empty index");

    let outcome = switch_repository(
        &fx.repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "collide".into(),
                oid: collide_tip,
            },
            dry_run: false,
            hooks: Default::default(),
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch");
    assert!(
        outcome
            .path_collisions
            .iter()
            .any(|p| p == "A.txt" || p == "a.txt"),
        "expected collision paths, got {:?}",
        outcome.path_collisions
    );
    let _ = fs::remove_file(fx.dest.join("A.txt"));
    if !fx.dest.join("a.txt").exists() {
        fs::write(fx.dest.join("a.txt"), b"twotwotwo\n").expect("a.txt");
    }

    let model = status(
        &fx.repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.iter().any(|e| e.path() == "A.txt"),
        "mismatched collision member must be unstaged: {:?}",
        model.unstaged.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

//! Large-repo grit pick / stash apply compatibility with system `git`.

use std::process::Command;

use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::stash::apply_stash;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_test_support::git;

const FILE_COUNT: usize = 3000;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn git_fsck(repo: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tree_of_head(repo: &std::path::Path) -> ObjectId {
    let hex = git_cmd(repo, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_owned();
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn populate(repo: &std::path::Path) {
    for i in 0..FILE_COUNT {
        let dir = repo.join(format!("d{:04}", i % 100));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("f{i:05}.txt")), format!("seed {i}\n")).expect("write");
    }
    git_cmd(repo, &["add", "-A"]);
    git_cmd(repo, &["commit", "-qm", "initial"]);
}

fn create_pick_commit(repo: &std::path::Path) {
    for i in (0..FILE_COUNT).step_by(3) {
        let dir = repo.join(format!("d{:04}", i % 100));
        std::fs::write(dir.join(format!("f{i:05}.txt")), format!("picked {i}\n")).expect("write");
    }
    git_cmd(repo, &["add", "-A"]);
    git_cmd(repo, &["commit", "-qm", "pick source"]);
}

fn grit_pick(repo: &Repository, source_hex: &str) -> grit_lib::error::Result<()> {
    let source_oid = ObjectId::from_hex(source_hex).expect("source");
    let source_obj = repo.odb.read(&source_oid)?;
    let source = grit_lib::objects::parse_commit(&source_obj.data)?;
    let head_oid = refs::resolve_ref(&repo.git_dir, "HEAD").expect("head");
    let head_obj = repo.odb.read(&head_oid)?;
    let head_commit = grit_lib::objects::parse_commit(&head_obj.data)?;
    let head_tree = head_commit.tree;
    let base_tree = if let Some(parent) = source.parents.first() {
        let pobj = repo.odb.read(parent)?;
        grit_lib::objects::parse_commit(&pobj.data)?.tree
    } else {
        repo.odb.write(ObjectKind::Tree, &[]).expect("empty tree")
    };
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        head_tree,
        source.tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )?;
    let mut index = merged.index;
    let new_tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;
    checkout_between_trees(repo, Some(&head_tree), &new_tree)?;
    let commit_data = CommitData {
        tree: new_tree,
        parents: vec![head_oid],
        author: source.author.clone(),
        committer: source.committer.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: source.message.clone(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let new_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit_data))?;
    refs::write_ref(&repo.git_dir, "HEAD", &new_oid).expect("update head");
    Ok(())
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn copy_repo(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir dest");
    let status = Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success(), "cp -a failed duplicating test repo");
}

#[test]
fn grit_pick_at_3k_matches_git_cherry_pick_tree() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    populate(base.path());
    let base_head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    create_pick_commit(base.path());
    let pick_source = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &base_head]);

    let grit_dir = tempfile::tempdir().expect("grit dir");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo =
        Repository::open(&grit_dir.path().join(".git"), Some(grit_dir.path())).expect("grit open");
    grit_pick(&grit_repo, &pick_source).expect("pick");
    let grit_tree = tree_of_head(grit_dir.path());

    let git_dir = tempfile::tempdir().expect("git dir");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["cherry-pick", &pick_source]);
    let git_tree = tree_of_head(git_dir.path());

    assert_eq!(
        grit_tree.to_hex(),
        git_tree.to_hex(),
        "grit pick must match git cherry-pick result tree"
    );
    assert!(
        git_cmd(grit_dir.path(), &["status", "--porcelain"])
            .trim()
            .is_empty(),
        "git status must be clean after grit pick"
    );
    git_fsck(grit_dir.path());
}

#[test]
fn grit_stash_apply_at_3k_matches_git_without_index() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    populate(base.path());
    let base_head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    for i in (0..FILE_COUNT).step_by(100) {
        let dir = base.path().join(format!("d{:04}", i % 100));
        std::fs::write(dir.join(format!("f{i:05}.txt")), format!("stashed {i}\n")).expect("write");
    }
    git_cmd(base.path(), &["add", "-A"]);
    git_cmd(base.path(), &["stash", "push", "-qm", "bench"]);
    let stash_oid_hex = git_cmd(base.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &base_head]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo =
        Repository::open(&grit_dir.path().join(".git"), Some(grit_dir.path())).expect("grit open");
    let stash_oid = ObjectId::from_hex(&stash_oid_hex).expect("stash oid");
    apply_stash(
        &grit_repo,
        grit_dir.path(),
        &stash_oid,
        false,
        "Test <t@example.com> 0 +0000",
    )
    .expect("stash apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    assert_eq!(
        git_cmd(grit_dir.path(), &["status", "--porcelain"]),
        git_cmd(git_dir.path(), &["status", "--porcelain"]),
        "3k-file apply without --index must match git porcelain"
    );
    let sample = grit_dir.path().join("d0000/f00000.txt");
    assert_eq!(
        std::fs::read(&sample).expect("sample bytes"),
        b"stashed 0\n"
    );
    git_fsck(grit_dir.path());
}

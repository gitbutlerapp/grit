//! Issue #1093: theirs-side rename + ours-side edit must merge at the rename target with edited content.

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_trees::merge_trees_three_way;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::fs;
use support::{
    commit_tree, git_ls_files_stage, git_ls_files_stage0_map, git_merge_tree_oracle,
    CommitMergeLabels,
};

fn issue_1093_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    let mut lines = String::new();
    for n in 1..=30 {
        lines.push_str(&format!("{n}\n"));
    }
    fs::write(root.join("a.txt"), &lines).expect("a.txt");
    fs::write(root.join("keep.txt"), "keep\n").expect("keep.txt");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "ren"]);
    git(root, &["mv", "a.txt", "b.txt"]);
    git(root, &["commit", "-qm", "rename"]);

    git(root, &["switch", "-q", "main"]);
    let mut edited = fs::read_to_string(root.join("a.txt")).expect("read a");
    edited = edited.replace("25\n", "twentyfive\n");
    fs::write(root.join("a.txt"), edited).expect("write a");
    git(root, &["commit", "-qam", "edit a"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

fn blob_line(root: &std::path::Path, tree: ObjectId, path: &str, line: u32) -> String {
    let spec = format!("{tree}:{path}");
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["show", &spec])
        .output()
        .expect("git show");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf8");
    text.lines()
        .nth((line - 1) as usize)
        .expect("line")
        .to_owned()
}

#[test]
fn merge_trees_rename_on_theirs_edit_on_ours_matches_git() {
    let (_tmp, repo) = issue_1093_repo();
    let root = repo.work_tree().expect("worktree");

    let base: ObjectId = git(root, &["merge-base", "main", "ren"])
        .trim()
        .parse()
        .unwrap();
    let ours: ObjectId = git(root, &["rev-parse", "main"]).trim().parse().unwrap();
    let theirs: ObjectId = git(root, &["rev-parse", "ren"]).trim().parse().unwrap();
    let base_tree = commit_tree(&repo, &base).expect("base tree");
    let ours_tree = commit_tree(&repo, &ours).expect("ours tree");
    let theirs_tree = commit_tree(&repo, &theirs).expect("theirs tree");

    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    let merged = merge_trees_three_way(
        &repo,
        base_tree,
        ours_tree,
        theirs_tree,
        Default::default(),
        Default::default(),
        None,
        labels.presentation(),
    )
    .expect("merge_trees_three_way");

    let git_tree = git_merge_tree_oracle(
        root,
        Some(&base),
        &ours,
        &theirs,
        grit_lib::diff::merge_file::MergeFavor::None,
    )
    .result_tree;
    assert_eq!(
        merged.result_tree, git_tree,
        "merge result tree must match git merge-tree"
    );
    assert_eq!(
        blob_line(root, merged.result_tree, "b.txt", 25),
        "twentyfive"
    );
}

#[test]
fn integrate_branch_rename_on_theirs_edit_on_ours_clean_index() {
    let (_tmp, repo) = issue_1093_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid: ObjectId = git(root, &["rev-parse", "main"]).trim().parse().unwrap();
    let ren_oid: ObjectId = git(root, &["rev-parse", "ren"]).trim().parse().unwrap();

    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, ren_oid, "ren").expect("merge");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected clean merge");
    };

    let merge_tree = commit_tree(&repo, &oid).expect("merge commit tree");
    assert_eq!(blob_line(root, merge_tree, "b.txt", 25), "twentyfive");

    let stage0 = git_ls_files_stage0_map(root);
    let b_oid = stage0.get("b.txt").expect("b.txt in index");
    assert_eq!(
        *b_oid,
        ObjectId::from_hex("998353851b12546269c5c0660a6433ea30a11fda").expect("expected blob")
    );
    let b_index_rows: Vec<_> = git_ls_files_stage(root)
        .into_iter()
        .filter(|(path, _, _, _)| path == "b.txt")
        .collect();
    assert_eq!(
        b_index_rows.len(),
        1,
        "index must have exactly one row for b.txt, got {b_index_rows:?}"
    );

    let status = git(root, &["status", "-s"]);
    assert!(
        status.trim().is_empty(),
        "worktree and index should be clean after merge, got:\n{status}"
    );
}

fn rename_on_theirs_delete_on_ours_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f.txt"), "content\n").expect("f.txt");
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "ren"]);
    git(root, &["mv", "f.txt", "g.txt"]);
    git(root, &["commit", "-qm", "rename"]);

    git(root, &["switch", "-q", "main"]);
    git(root, &["rm", "f.txt"]);
    git(root, &["commit", "-qm", "delete"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

/// Bugbot: skipping a rename target when the source is only in `touched` must not run when ours
/// deleted the source — `merge_one_path` can emit nothing and the renamed file would vanish.
#[test]
fn merge_trees_rename_on_theirs_delete_on_ours_matches_git() {
    let (_tmp, repo) = rename_on_theirs_delete_on_ours_repo();
    let root = repo.work_tree().expect("worktree");

    let base: ObjectId = git(root, &["merge-base", "main", "ren"])
        .trim()
        .parse()
        .unwrap();
    let ours: ObjectId = git(root, &["rev-parse", "main"]).trim().parse().unwrap();
    let theirs: ObjectId = git(root, &["rev-parse", "ren"]).trim().parse().unwrap();
    let base_tree = commit_tree(&repo, &base).expect("base tree");
    let ours_tree = commit_tree(&repo, &ours).expect("ours tree");
    let theirs_tree = commit_tree(&repo, &theirs).expect("theirs tree");

    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    let merged = merge_trees_three_way(
        &repo,
        base_tree,
        ours_tree,
        theirs_tree,
        Default::default(),
        Default::default(),
        None,
        labels.presentation(),
    )
    .expect("merge_trees_three_way");

    let oracle = git_merge_tree_oracle(
        root,
        Some(&base),
        &ours,
        &theirs,
        grit_lib::diff::merge_file::MergeFavor::None,
    );
    assert_eq!(
        merged.result_tree, oracle.result_tree,
        "merge result tree must match git merge-tree"
    );
}

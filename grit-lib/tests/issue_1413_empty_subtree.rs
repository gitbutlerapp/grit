//! Issue #1413: merged/replayed trees must not retain directory entries for empty subtrees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;

use grit_lib::diff::merge_trees::{merge_trees_three_way, TreeMergeConflictPresentation};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::revwalk::history::{
    apply_rewrite, drop as history_drop, ApplyOptions, EmptyPolicy, RefScope, RewriteOptions,
};

use support::{
    assert_git_fsck_strict, commit_file, commit_tree, git, git_fsck_strict, git_rev_parse,
    open_grit_repo, HistoryRepo,
};

const EMPTY_TREE_CANON: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn seed_three_dir_commits(wt: &Path) -> ObjectId {
    commit_file(wt, "d1/f.txt", "v1\n", "c1");
    commit_file(wt, "d2/f.txt", "v1\n", "c2");
    commit_file(wt, "d3/f.txt", "v1\n", "c3");
    git_rev_parse(wt, "HEAD").expect("tip")
}

fn assert_head_has_no_empty_subtree_dirs(wt: &Path) {
    let ls = git(wt, &["ls-tree", "HEAD"]);
    for line in ls.lines() {
        let mut parts = line.split_whitespace();
        let _mode = parts.next();
        let kind = parts.next().unwrap_or("");
        let oid = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        if kind == "tree" {
            assert_ne!(
                oid, EMPTY_TREE_CANON,
                "directory {name} points at canonical empty tree"
            );
        }
        assert!(
            !line.contains(EMPTY_TREE_CANON),
            "HEAD tree lists empty subtree: {line}"
        );
    }
    assert!(
        !ls.contains("\td2\t"),
        "emptied directory d2 still in HEAD tree: {ls}"
    );
    git_fsck_strict(wt);
}

fn drop_opts(seq: i64) -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: format!("Tester <t@grit.test> {seq} +0000"),
        preserve_committer: false,
        sign: true,
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Tester <t@grit.test>".into(),
        reflog_message: "history drop".into(),
        record_operation: None,
        hooks: grit_lib::PostOperationHooks::Run,
    }
}

#[test]
fn history_drop_emptied_subdirectory_fsck_clean() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    seed_three_dir_commits(wt);
    let c2 = git_rev_parse(wt, "HEAD~1").expect("c2");
    let grit = open_grit_repo(wt);
    let plan = history_drop(&grit, c2, &drop_opts(0)).expect("drop c2");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    assert_head_has_no_empty_subtree_dirs(wt);
}

#[test]
fn replay_revert_emptied_subdirectory_fsck_clean() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    seed_three_dir_commits(wt);
    let c2 = git_rev_parse(wt, "HEAD~1").expect("c2");
    let grit = open_grit_repo(wt);
    let outcome = replay_commit(
        &grit,
        &ReplayRequest {
            commit: c2,
            direction: ReplayDirection::Revert,
            committer: "Tester <t@grit.test> 1700000060 +0000".into(),
            message_override: None,
        },
    )
    .expect("revert c2");
    assert!(matches!(outcome, ReplayOutcome::Committed { .. }));
    assert_head_has_no_empty_subtree_dirs(wt);
}

#[test]
fn replay_pick_dir_delete_fsck_clean() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "d2/f.txt", "v1\n", "add d2");
    let delete_commit = {
        std::fs::remove_file(wt.join("d2/f.txt")).expect("rm");
        std::fs::remove_dir(wt.join("d2")).expect("rmdir");
        git(wt, &["add", "-A"]);
        git(wt, &["commit", "-m", "remove d2"]);
        git_rev_parse(wt, "HEAD").expect("delete")
    };
    git(wt, &["reset", "--hard", "HEAD~1"]);
    let grit = open_grit_repo(wt);
    let outcome = replay_commit(
        &grit,
        &ReplayRequest {
            commit: delete_commit,
            direction: ReplayDirection::Pick,
            committer: "Tester <t@grit.test> 1700000120 +0000".into(),
            message_override: None,
        },
    )
    .expect("pick delete");
    assert!(matches!(outcome, ReplayOutcome::Committed { .. }));
    assert_head_has_no_empty_subtree_dirs(wt);
}

#[test]
fn merge_branch_that_empties_subdirectory_fsck_clean() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "gone/only.txt", "only\n", "base gone");
    git(wt, &["switch", "-c", "del"]);
    std::fs::remove_file(wt.join("gone/only.txt")).expect("rm");
    std::fs::remove_dir(wt.join("gone")).expect("rmdir");
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "delete gone"]);
    git(wt, &["switch", "main"]);
    commit_file(wt, "other.txt", "other\n", "add other");
    let grit = open_grit_repo(wt);
    let base_commit = git(wt, &["merge-base", "main", "del"])
        .trim()
        .parse::<ObjectId>()
        .expect("merge-base");
    let ours_commit = git_rev_parse(wt, "HEAD").expect("ours");
    let theirs_commit = git_rev_parse(wt, "refs/heads/del").expect("theirs");
    let base_tree = commit_tree(&grit, &base_commit).expect("base tree");
    let ours_tree = commit_tree(&grit, &ours_commit).expect("ours tree");
    let theirs_tree = commit_tree(&grit, &theirs_commit).expect("theirs tree");
    let merged = merge_trees_three_way(
        &grit,
        base_tree,
        ours_tree,
        theirs_tree,
        Default::default(),
        Default::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .expect("merge trees");
    assert!(
        merged.conflict_content.is_empty(),
        "unexpected conflicts: {:?}",
        merged.conflict_content
    );
    let tree_hex = merged.result_tree.to_hex();
    let ls = git(wt, &["ls-tree", &tree_hex]);
    assert!(
        !ls.contains(EMPTY_TREE_CANON),
        "merged tree retains empty subtree: {ls}"
    );
    assert!(
        !ls.contains("\tgone\t"),
        "merged tree still has gone/: {ls}"
    );
    assert_git_fsck_strict(wt);
}

//! Issue #1878: a base file renamed to different paths on each side (rename/rename 1to2)
//! must stop with a conflict, the same index stages and the same result tree as Git.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{merge_trees_three_way, RenameRenameConflict};
use grit_lib::objects::ObjectId;
use grit_lib::operation::{conflicts_from_tree_merge, Conflict, ConflictKind};
use grit_lib::porcelain::merge::{
    integrate_branch_with, predict_merge, IntegrateBranchOutcome, MergeOptions, MergePrediction,
    MergePredictionOptions,
};
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    rebase_onto, EmptyPolicy, HistoryError, RebaseOntoOptions, RefScope, RewriteOptions,
};
use grit_test_support::git;
use support::{
    assert_git_fsck_strict, assert_merge_output_matches_oracle, commit_tree, git_ls_files_stage,
    git_merge_tree_oracle, CommitMergeLabels,
};

const BASE: &str = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
const COMMITTER: &str = "Rename Test <rr@example.com> 1700000000 +0000";

/// How each side edits the file while renaming it.
#[derive(Clone, Copy)]
enum Edit {
    None,
    Top(&'static str),
    Bottom(&'static str),
}

fn apply_edit(path: &Path, edit: Edit) {
    let text = fs::read_to_string(path).expect("read");
    let text = match edit {
        Edit::None => text,
        Edit::Top(line) => text.replacen("line1\n", &format!("{line}\n"), 1),
        Edit::Bottom(line) => text.replacen("line8\n", &format!("{line}\n"), 1),
    };
    fs::write(path, text).expect("write");
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: Repository,
    base: ObjectId,
    main: ObjectId,
    topic: ObjectId,
}

impl Fixture {
    fn root(&self) -> &Path {
        self.repo.work_tree().expect("work tree")
    }
}

/// `a` renamed to `b` on `main` and to `c` on `topic`, with optional edits on each side.
fn rename_rename_repo(ours: Edit, theirs: Edit) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), BASE).expect("write a");
    fs::write(root.join("keep"), "keep\n").expect("write keep");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-qc", "topic"]);
    git(root, &["mv", "a", "c"]);
    apply_edit(&root.join("c"), theirs);
    git(root, &["commit", "-qam", "a->c"]);

    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "a", "b"]);
    apply_edit(&root.join("b"), ours);
    git(root, &["commit", "-qam", "a->b"]);

    let rev = |spec: &str| -> ObjectId { git(root, &["rev-parse", spec]).trim().parse().unwrap() };
    let base = rev("main~1");
    let main = rev("main");
    let topic = rev("topic");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    Fixture {
        _tmp: tmp,
        repo,
        base,
        main,
        topic,
    }
}

fn expected_kind() -> ConflictKind {
    ConflictKind::RenameRename {
        base_path: "a".into(),
        ours_path: "b".into(),
        theirs_path: "c".into(),
    }
}

fn expected_conflicts() -> Vec<Conflict> {
    ["a", "b", "c"]
        .into_iter()
        .map(|path| Conflict {
            path: path.into(),
            kind: expected_kind(),
        })
        .collect()
}

/// Merge the trees directly and compare result tree, stages and blobs with `git merge-tree`.
fn assert_tree_merge_matches_git(fx: &Fixture, context: &str) {
    let labels = CommitMergeLabels::from_commits(&fx.main, &fx.topic);
    let merged = merge_trees_three_way(
        &fx.repo,
        commit_tree(&fx.repo, &fx.base).unwrap(),
        commit_tree(&fx.repo, &fx.main).unwrap(),
        commit_tree(&fx.repo, &fx.topic).unwrap(),
        MergeFavor::None,
        Default::default(),
        None,
        labels.presentation(),
    )
    .expect("merge trees");
    assert_eq!(
        merged.rename_conflicts,
        vec![RenameRenameConflict {
            base_path: b"a".to_vec(),
            ours_path: b"b".to_vec(),
            theirs_path: b"c".to_vec(),
        }],
        "{context}"
    );
    assert_eq!(
        conflicts_from_tree_merge(&merged),
        expected_conflicts(),
        "{context}"
    );
    let oracle = git_merge_tree_oracle(
        fx.root(),
        Some(&fx.base),
        &fx.main,
        &fx.topic,
        MergeFavor::None,
    );
    assert!(
        oracle.stdout.contains("CONFLICT (rename/rename)"),
        "{context}: git must report rename/rename:\n{}",
        oracle.stdout
    );
    assert_merge_output_matches_oracle(&fx.repo, &merged, &oracle, context);
}

#[test]
fn tree_merge_matches_git_without_edits() {
    let fx = rename_rename_repo(Edit::None, Edit::None);
    assert_tree_merge_matches_git(&fx, "plain rename/rename");
}

#[test]
fn tree_merge_matches_git_when_ours_edits() {
    let fx = rename_rename_repo(Edit::Top("LINE1-ours"), Edit::None);
    assert_tree_merge_matches_git(&fx, "ours edits");
}

#[test]
fn tree_merge_matches_git_when_theirs_edits() {
    let fx = rename_rename_repo(Edit::None, Edit::Bottom("LINE8-theirs"));
    assert_tree_merge_matches_git(&fx, "theirs edits");
}

#[test]
fn tree_merge_matches_git_when_both_edit_cleanly() {
    let fx = rename_rename_repo(Edit::Top("LINE1-ours"), Edit::Bottom("LINE8-theirs"));
    assert_tree_merge_matches_git(&fx, "both edit, clean content merge");
}

#[test]
fn tree_merge_matches_git_when_edits_overlap() {
    let fx = rename_rename_repo(Edit::Top("LINE1-ours"), Edit::Top("LINE1-theirs"));
    assert_tree_merge_matches_git(&fx, "both edit, conflicting content");
}

#[test]
fn predict_merge_reports_rename_rename() {
    let fx = rename_rename_repo(Edit::None, Edit::None);
    let prediction = predict_merge(
        &fx.repo,
        fx.main,
        fx.topic,
        &MergePredictionOptions::default(),
    )
    .expect("predict");
    assert_eq!(
        prediction,
        MergePrediction::Conflicts {
            conflicts: expected_conflicts()
        }
    );
}

#[test]
fn integrate_branch_stops_with_git_index_stages() {
    let fx = rename_rename_repo(Edit::Top("LINE1-ours"), Edit::Bottom("LINE8-theirs"));
    let root = fx.root();
    let outcome = integrate_branch_with(
        &fx.repo,
        "refs/heads/main",
        fx.main,
        fx.topic,
        "topic",
        &MergeOptions::default(),
    )
    .expect("merge");
    let IntegrateBranchOutcome::Conflicts { paths, details } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(paths, vec!["a", "b", "c"]);
    assert_eq!(details, expected_conflicts());
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        fx.main.to_hex(),
        "branch must not move"
    );

    let merged = BASE
        .replacen("line1", "LINE1-ours", 1)
        .replacen("line8", "LINE8-theirs", 1);
    let base_blob: ObjectId = git(root, &["rev-parse", "main~1:a"])
        .trim()
        .parse()
        .unwrap();
    let rows: Vec<(String, u32)> = git_ls_files_stage(root)
        .into_iter()
        .filter(|(path, ..)| path != "keep")
        .map(|(path, stage, _, oid)| {
            if path == "a" {
                assert_eq!(oid, base_blob, "a@1 must be the base blob");
            } else {
                let body = git(root, &["cat-file", "-p", &oid.to_hex()]);
                assert_eq!(body, merged, "{path}@{stage} must hold the merged content");
            }
            (path, stage)
        })
        .collect();
    assert_eq!(
        rows,
        vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)],
        "index stages must match git's ort layout"
    );
    assert_eq!(fs::read_to_string(root.join("b")).unwrap(), merged);
    assert_eq!(fs::read_to_string(root.join("c")).unwrap(), merged);
    assert!(!root.join("a").exists());
    let status = git(root, &["status", "--porcelain"]);
    assert!(status.contains("DD a"), "{status}");
    assert!(status.contains("AU b"), "{status}");
    assert!(status.contains("UA c"), "{status}");
    assert_git_fsck_strict(root);
}

#[test]
fn pick_of_the_other_rename_conflicts() {
    let fx = rename_rename_repo(Edit::None, Edit::None);
    let outcome = replay_commit(
        &fx.repo,
        &ReplayRequest {
            commit: fx.topic,
            direction: ReplayDirection::Pick,
            committer: COMMITTER.parse().expect("COMMITTER"),
            message_override: None,
        },
    )
    .expect("pick");
    let ReplayOutcome::Conflicts { paths } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(paths, vec!["a", "b", "c"]);
}

#[test]
fn rebase_onto_the_other_rename_conflicts() {
    let fx = rename_rename_repo(Edit::None, Edit::None);
    let opts = RebaseOntoOptions::new(RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: COMMITTER.parse().expect("signature"),
        preserve_committer: false,
        sign: true,
    });
    let err = rebase_onto(&fx.repo, fx.base, fx.topic, &[fx.main], &opts)
        .expect_err("rebase must stop on the rename/rename conflict");
    let HistoryError::Conflict { commit, paths } = err else {
        panic!("expected a conflict, got {err:?}");
    };
    assert_eq!(commit, fx.main);
    assert_eq!(paths, vec!["a", "b", "c"]);
}

/// Both sides renamed `a` to `b` (rename/rename 1to1) and edited different lines.
#[test]
fn same_rename_on_both_sides_merges_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), BASE).expect("write a");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "topic"]);
    git(root, &["mv", "a", "b"]);
    apply_edit(&root.join("b"), Edit::Bottom("LINE8-theirs"));
    git(root, &["commit", "-qam", "a->b theirs"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "a", "b"]);
    apply_edit(&root.join("b"), Edit::Top("LINE1-ours"));
    git(root, &["commit", "-qam", "a->b ours"]);
    assert_clean_merge_matches_git(root);
}

/// Our side renamed `a` to `b`; theirs edited `a` in place (the mirror of issue #1093).
#[test]
fn rename_on_ours_edit_on_theirs_merges_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), BASE).expect("write a");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "topic"]);
    apply_edit(&root.join("a"), Edit::Bottom("LINE8-theirs"));
    git(root, &["commit", "-qam", "edit a"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "a", "b"]);
    git(root, &["commit", "-qam", "a->b"]);
    assert_clean_merge_matches_git(root);
}

/// `main` and `topic` must merge cleanly, in memory and for real, to Git's tree.
fn assert_clean_merge_matches_git(root: &Path) {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let rev = |spec: &str| -> ObjectId { git(root, &["rev-parse", spec]).trim().parse().unwrap() };
    let (main, topic) = (rev("main"), rev("topic"));
    let oracle = git_merge_tree_oracle(root, None, &main, &topic, MergeFavor::None);
    assert!(oracle.unmerged.is_empty(), "git: {}", oracle.stdout);
    let prediction =
        predict_merge(&repo, main, topic, &MergePredictionOptions::default()).expect("predict");
    assert_eq!(
        prediction,
        MergePrediction::Clean {
            tree: oracle.result_tree
        }
    );
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions::default(),
    )
    .expect("merge");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected a clean merge, got {outcome:?}");
    };
    assert_eq!(commit_tree(&repo, &oid).unwrap(), oracle.result_tree);
    assert!(git(root, &["status", "--porcelain"]).is_empty());
    assert_git_fsck_strict(root);
}

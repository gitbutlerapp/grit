//! Issue #2112: when a rename lands on a path the other side also filled (rename/rename 2to1,
//! rename/add), both versions must stay unmerged at the destination, as in Git, instead of one
//! silently replacing the other.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::merge_trees_three_way;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind};
use grit_lib::porcelain::merge::{
    integrate_branch_with, predict_merge, IntegrateBranchOutcome, MergeOptions, MergePrediction,
    MergePredictionOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::git;
use support::{
    assert_git_fsck_strict, assert_merge_output_matches_oracle, commit_tree, git_ls_files_stage,
    git_merge_tree_oracle, CommitMergeLabels,
};

const A: &str = "a1\na2\na3\na4\n";
const B: &str = "b1\nb2\nb3\nb4\n";

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

/// Replace `from` with `to` in the work-tree file `path` and stage it.
fn edit(root: &Path, path: &str, from: &str, to: &str) {
    let text = fs::read_to_string(root.join(path)).expect("read");
    fs::write(root.join(path), text.replacen(from, to, 1)).expect("write");
    git(root, &["add", path]);
}

/// Base commit with files `a` and `b`; `topic` and `main` each apply their own changes on top.
fn repo(topic: impl FnOnce(&Path), main: impl FnOnce(&Path)) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), A).expect("write a");
    fs::write(root.join("b"), B).expect("write b");
    git(root, &["add", "a", "b"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "topic"]);
    topic(root);
    git(root, &["commit", "-qm", "topic"]);
    git(root, &["switch", "-q", "main"]);
    main(root);
    git(root, &["commit", "-qm", "main"]);

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

/// The issue's reproduction: `main` renames `a` to `c`, `topic` renames `b` to `c`.
fn two_to_one() -> Fixture {
    repo(
        |root| {
            git(root, &["mv", "b", "c"]);
        },
        |root| {
            git(root, &["mv", "a", "c"]);
        },
    )
}

fn add_add_on_c() -> Vec<Conflict> {
    vec![Conflict {
        path: "c".into(),
        kind: ConflictKind::AddAdd,
    }]
}

fn blob(root: &Path, spec: &str) -> ObjectId {
    git(root, &["rev-parse", spec]).trim().parse().unwrap()
}

/// Merge `main` and `topic` in memory and require Git's result tree, stages and blobs.
///
/// Returns the stage-2 and stage-3 blob ids at `c`.
fn assert_tree_merge_matches_git(fx: &Fixture, context: &str) -> (ObjectId, ObjectId) {
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
    let oracle = git_merge_tree_oracle(
        fx.root(),
        Some(&fx.base),
        &fx.main,
        &fx.topic,
        MergeFavor::None,
    );
    assert!(
        oracle.stdout.contains("CONFLICT (add/add)"),
        "{context}: git must report add/add:\n{}",
        oracle.stdout
    );
    assert_merge_output_matches_oracle(&fx.repo, &merged, &oracle, context);
    let stages: Vec<(u32, &str, ObjectId)> = oracle
        .unmerged
        .iter()
        .map(|row| (row.stage, row.path.as_str(), row.oid))
        .collect();
    let [(2, "c", ours), (3, "c", theirs)] = stages[..] else {
        panic!("{context}: expected stages 2 and 3 at c, got {stages:?}");
    };
    (ours, theirs)
}

fn assert_predicts_add_add(fx: &Fixture) {
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
            conflicts: add_add_on_c()
        }
    );
}

#[test]
fn two_files_renamed_to_one_path_keep_both_versions() {
    let fx = two_to_one();
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "pure rename/rename 2to1");
    assert_eq!(ours, blob(fx.root(), "main~1:a"));
    assert_eq!(theirs, blob(fx.root(), "main~1:b"));
    assert_predicts_add_add(&fx);
}

#[test]
fn edit_of_the_renamed_source_on_the_other_side_is_merged_into_its_stage() {
    let fx = repo(
        |root| {
            git(root, &["mv", "b", "c"]);
            edit(root, "a", "a2", "A2-theirs");
        },
        |root| {
            git(root, &["mv", "a", "c"]);
        },
    );
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "theirs edits a");
    assert_eq!(ours, blob(fx.root(), "topic:a"));
    assert_eq!(theirs, blob(fx.root(), "topic:c"));
    assert_predicts_add_add(&fx);
}

#[test]
fn edit_on_our_side_is_merged_into_their_renames_stage() {
    let fx = repo(
        |root| {
            git(root, &["mv", "b", "c"]);
        },
        |root| {
            git(root, &["mv", "a", "c"]);
            edit(root, "b", "b2", "B2-ours");
        },
    );
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "ours edits b");
    assert_eq!(ours, blob(fx.root(), "main:c"));
    assert_eq!(theirs, blob(fx.root(), "main:b"));
    assert_predicts_add_add(&fx);
}

#[test]
fn conflicting_edits_of_the_source_nest_markers_at_the_destination() {
    let fx = repo(
        |root| {
            git(root, &["mv", "b", "c"]);
            edit(root, "a", "a2", "A2-theirs");
        },
        |root| {
            git(root, &["mv", "a", "c"]);
            edit(root, "c", "a2", "A2-ours");
        },
    );
    let (ours, _) = assert_tree_merge_matches_git(&fx, "both edit a");
    let inner = String::from_utf8(fx.repo.odb().read(&ours).unwrap().data).unwrap();
    assert!(
        inner.contains("<<<<<<<< ") && inner.contains("A2-ours") && inner.contains("A2-theirs"),
        "stage 2 must hold the inner conflict:\n{inner}"
    );
    assert_predicts_add_add(&fx);
}

#[test]
fn rename_onto_a_path_the_other_side_added_conflicts() {
    let fx = repo(
        |root| {
            fs::write(root.join("c"), "x1\nx2\nx3\n").expect("write c");
            git(root, &["add", "c"]);
        },
        |root| {
            git(root, &["mv", "a", "c"]);
        },
    );
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "rename/add");
    assert_eq!(ours, blob(fx.root(), "main~1:a"));
    assert_eq!(theirs, blob(fx.root(), "topic:c"));
    assert_predicts_add_add(&fx);
}

#[test]
fn add_onto_a_path_the_other_side_renamed_to_conflicts() {
    let fx = repo(
        |root| {
            git(root, &["mv", "a", "c"]);
        },
        |root| {
            fs::write(root.join("c"), "x1\nx2\nx3\n").expect("write c");
            git(root, &["add", "c"]);
        },
    );
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "add/rename");
    assert_eq!(ours, blob(fx.root(), "main:c"));
    assert_eq!(theirs, blob(fx.root(), "main~1:a"));
    assert_predicts_add_add(&fx);
}

#[test]
fn rename_whose_source_the_other_side_deleted_still_conflicts() {
    let fx = repo(
        |root| {
            git(root, &["mv", "b", "c"]);
            git(root, &["rm", "-q", "a"]);
        },
        |root| {
            git(root, &["mv", "a", "c"]);
        },
    );
    let (ours, theirs) = assert_tree_merge_matches_git(&fx, "rename/delete into a collision");
    assert_eq!(ours, blob(fx.root(), "main~1:a"));
    assert_eq!(theirs, blob(fx.root(), "main~1:b"));
    assert_predicts_add_add(&fx);
}

#[test]
fn rename_onto_identical_added_content_merges_cleanly() {
    let fx = repo(
        |root| {
            fs::write(root.join("c"), A).expect("write c");
            git(root, &["add", "c"]);
        },
        |root| {
            git(root, &["mv", "a", "c"]);
        },
    );
    let oracle = git_merge_tree_oracle(fx.root(), None, &fx.main, &fx.topic, MergeFavor::None);
    assert!(oracle.unmerged.is_empty(), "git: {}", oracle.stdout);
    let prediction = predict_merge(
        &fx.repo,
        fx.main,
        fx.topic,
        &MergePredictionOptions::default(),
    )
    .expect("predict");
    assert_eq!(
        prediction,
        MergePrediction::Clean {
            tree: oracle.result_tree
        }
    );
}

#[test]
fn integrate_branch_stops_with_both_versions_in_the_index() {
    let fx = two_to_one();
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
    assert_eq!(paths, vec!["c"]);
    assert_eq!(details, add_add_on_c());
    assert_eq!(
        git(root, &["rev-parse", "main"]).trim(),
        fx.main.to_hex(),
        "branch must not move"
    );
    let rows: Vec<(String, u32, ObjectId)> = git_ls_files_stage(root)
        .into_iter()
        .map(|(path, stage, _, oid)| (path, stage, oid))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("c".into(), 2, blob(root, "main~1:a")),
            ("c".into(), 3, blob(root, "main~1:b")),
        ]
    );
    let work = fs::read_to_string(root.join("c")).unwrap();
    assert!(
        work.contains(A) && work.contains(B) && work.contains("<<<<<<< "),
        "work tree must show both versions with markers:\n{work}"
    );
    assert!(!root.join("a").exists() && !root.join("b").exists());
    let status = git(root, &["status", "--porcelain"]);
    assert!(status.contains("D  b"), "{status}");
    assert!(status.contains("AA c"), "{status}");
    assert_git_fsck_strict(root);
}

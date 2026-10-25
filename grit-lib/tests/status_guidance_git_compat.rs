//! `StatusModel::is_clean`, `conflict_groups` and `guidance` on repositories that system `git`
//! put into each state (merge conflicts of several kinds, a dirty worktree, a branch behind its
//! upstream).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{
    status, ConflictGroup, ConflictKind, StatusAction, StatusGuidance, StatusModel, StatusOptions,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::branch_tracking::{BranchStanding, StandingOptions};
use grit_lib::repo::Repository;
use grit_lib::state::{InProgressOperation, NextStep};
use grit_test_support::{git, git_cmd};
use std::path::Path;
use tempfile::TempDir;

fn init(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
}

fn write(root: &Path, path: &str, content: &str) {
    std::fs::write(root.join(path), content).expect("write file");
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo")
}

fn model_and_standing(repo: &Repository) -> (StatusModel, BranchStanding) {
    let model = status(
        repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    let standing = BranchStanding::compute(
        repo,
        &model.head,
        StandingOptions {
            newest_ahead_limit: 10,
            counts: true,
        },
    )
    .expect("standing");
    (model, standing)
}

/// `both.txt` is modified on both sides, `gone-there.txt` is deleted by the merged branch and
/// `gone-here.txt` by the current one, each while the other side modifies it.
fn conflicted_merge() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    init(root);
    for path in ["both.txt", "gone-there.txt", "gone-here.txt"] {
        write(root, path, "base\n");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    write(root, "both.txt", "side\n");
    write(root, "gone-here.txt", "side\n");
    git(root, &["rm", "-q", "gone-there.txt"]);
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    write(root, "both.txt", "main\n");
    write(root, "gone-there.txt", "main\n");
    git(root, &["rm", "-q", "gone-here.txt"]);
    git(root, &["commit", "-qam", "main"]);
    let merge = git_cmd(&["merge", "side"]).in_dir(root).exec();
    assert_ne!(merge.status, Some(0), "expected merge conflicts");
    tmp
}

#[test]
fn merge_conflicts_group_by_kind_and_point_at_resolution() {
    let tmp = conflicted_merge();
    let repo = open(tmp.path());
    let (model, standing) = model_and_standing(&repo);

    assert!(!model.is_clean());
    assert_eq!(
        model.conflict_groups(),
        vec![
            ConflictGroup {
                kind: ConflictKind::BothModified,
                paths: vec!["both.txt".to_owned()],
            },
            ConflictGroup {
                kind: ConflictKind::DeletedByUs,
                paths: vec!["gone-here.txt".to_owned()],
            },
            ConflictGroup {
                kind: ConflictKind::DeletedByThem,
                paths: vec!["gone-there.txt".to_owned()],
            },
        ]
    );
    assert_eq!(
        model.guidance(&standing),
        StatusGuidance::Operation {
            steps: vec![NextStep::ResolveConflicts, NextStep::StageResolved],
            alternative: Some(NextStep::Abort(InProgressOperation::Merge)),
        }
    );
}

#[test]
fn resolved_merge_is_not_clean_and_suggests_continue() {
    let tmp = conflicted_merge();
    let root = tmp.path();
    write(root, "both.txt", "resolved\n");
    git(root, &["add", "both.txt", "gone-there.txt"]);
    git(root, &["rm", "-q", "gone-here.txt"]);
    let repo = open(root);
    let (model, standing) = model_and_standing(&repo);

    assert!(model.conflict_groups().is_empty());
    assert!(!model.is_clean(), "a merge in progress is never clean");
    assert_eq!(
        model.guidance(&standing),
        StatusGuidance::Operation {
            steps: vec![NextStep::Continue(InProgressOperation::Merge)],
            alternative: Some(NextStep::Abort(InProgressOperation::Merge)),
        }
    );
}

#[test]
fn dirty_worktree_suggests_discard_stage_and_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    init(root);
    write(root, "tracked.txt", "one\n");
    write(root, "staged.txt", "one\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    write(root, "tracked.txt", "two\n");
    write(root, "staged.txt", "two\n");
    git(root, &["add", "staged.txt"]);
    write(root, "new.txt", "new\n");
    let repo = open(root);
    let (model, standing) = model_and_standing(&repo);

    assert!(!model.is_clean());
    assert_eq!(
        model.guidance(&standing),
        StatusGuidance::Actions(vec![
            StatusAction::DiscardWorktreeChanges,
            StatusAction::Stage,
            StatusAction::Commit,
        ])
    );
}

#[test]
fn clean_clone_behind_its_upstream_suggests_pull() {
    let tmp = TempDir::new().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir(&origin).expect("mkdir origin");
    init(&origin);
    write(&origin, "f", "one\n");
    git(&origin, &["add", "f"]);
    git(&origin, &["commit", "-qm", "one"]);
    let clone = tmp.path().join("clone");
    git(
        tmp.path(),
        &["clone", "-q", origin.to_str().expect("utf-8 path"), "clone"],
    );
    write(&origin, "f", "two\n");
    git(&origin, &["commit", "-qam", "two"]);
    git(&clone, &["fetch", "-q"]);

    let repo = open(&clone);
    let (model, standing) = model_and_standing(&repo);
    assert!(model.is_clean());
    assert_eq!(
        model.guidance(&standing),
        StatusGuidance::Actions(vec![StatusAction::Pull {
            from: "origin/main".to_owned()
        }])
    );

    git(&clone, &["merge", "-q", "--ff-only", "origin/main"]);
    let repo = open(&clone);
    let (model, standing) = model_and_standing(&repo);
    assert!(model.is_clean());
    assert_eq!(
        model.guidance(&standing),
        StatusGuidance::Actions(Vec::new())
    );
}

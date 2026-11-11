//! Issue #2179: directory rename detection in three-way merges, checked against
//! `git merge-tree --write-tree` for every `merge.directoryRenames` value, and round-tripped
//! through the index and work tree with the system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::progress::{CancelToken, NullProgress};

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, DirectoryRenameConflict, DirectoryRenameMode, MergeSide,
};
use grit_lib::merge::{
    IntegrateBranchOutcome, MergeDryRunPlan, MergeOptions, MergePrediction, MergePredictionOptions,
    MergeTreeStage,
};
use grit_lib::objects::ObjectId;
use grit_lib::operation::{
    conflicts_from_index, conflicts_from_tree_merge, tree_merge_has_conflicts, Conflict,
    ConflictKind, MovedPath,
};
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::repo::Repository;
use grit_test_support::git;
use grit_test_support::{
    abort_branch_merge, integrate_branch_with, plan_branch_merge, predict_merge, write_merge_tree,
};
use support::{
    assert_git_fsck_strict, assert_merge_output_matches_oracle, commit_tree, git_ls_files_stage,
    git_merge_tree_oracle, CommitMergeLabels,
};

const COMMITTER: &str = "Dir Rename <dr@example.com> 1700000000 +0000";

/// Directory shapes from the issue and Git's directory rename rules.
#[derive(Clone, Copy, Debug)]
enum Scenario {
    /// Ours renames `a/` to `b/`; theirs adds `a/new` (the reported case).
    Basic,
    /// Theirs renames `a/` to `b/`; ours adds `a/new`.
    Reverse,
    /// Ours spreads `a/` over `b/` and `c/` evenly; theirs adds `a/new`.
    Split,
    /// Ours sends two of four files to `b/` and one each to `c/` and `d/`.
    Plurality,
    /// Ours keeps one file in `a/`, so the directory is not renamed.
    Partial,
    /// Theirs adds `a/new` and an unrelated `b/new`.
    Blocked,
    /// Ours merges `a/` and `c/` into `b/`; theirs adds `a/n` and `c/n`.
    Collision,
    /// Ours renames `p/` (holding `p/a/`) to `r/`; theirs adds `p/new` and `p/a/new`.
    Nested,
    /// Ours moves `a/*` to the repository root; theirs adds `a/new`.
    ToRoot,
    /// Theirs renames `q/f` into `a/` while ours renames `a/` to `b/`.
    RenamedInto,
    /// As [`Self::RenamedInto`], with each side editing a different line of the file.
    RenamedIntoEdited,
    /// Theirs adds a file in a new subdirectory only; `a/` itself gets nothing new.
    NewSubdirectoryOnly,
    /// The moved file's destination `b/new` is a base file each side keeps, deletes or edits.
    Destination(DestinationChange, DestinationChange),
    /// Ours renames `a/` to `b/` and adds its own `b/new`; theirs adds `a/new`.
    DestinationAddedByRenamer,
}

/// What one side does to `b/new`, a base file at the destination of theirs' `a/new` after ours
/// moves `a/*` into the existing `b/`.
#[derive(Clone, Copy, Debug)]
enum DestinationChange {
    Keep,
    Delete,
    Edit,
}

const SCENARIOS: [Scenario; 18] = [
    Scenario::Basic,
    Scenario::Reverse,
    Scenario::Split,
    Scenario::Plurality,
    Scenario::Partial,
    Scenario::Blocked,
    Scenario::Collision,
    Scenario::Nested,
    Scenario::ToRoot,
    Scenario::RenamedInto,
    Scenario::RenamedIntoEdited,
    Scenario::NewSubdirectoryOnly,
    Scenario::Destination(DestinationChange::Delete, DestinationChange::Delete),
    Scenario::Destination(DestinationChange::Keep, DestinationChange::Delete),
    Scenario::Destination(DestinationChange::Edit, DestinationChange::Delete),
    Scenario::Destination(DestinationChange::Delete, DestinationChange::Keep),
    Scenario::Destination(DestinationChange::Delete, DestinationChange::Edit),
    Scenario::DestinationAddedByRenamer,
];

struct Fixture {
    _tmp: tempfile::TempDir,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
}

impl Fixture {
    fn root(&self) -> &Path {
        self._tmp.path()
    }

    /// A fresh handle, so config written by `git config` is picked up.
    fn repo(&self) -> Repository {
        Repository::discover(Some(self.root()), &RepositoryOptions::empty()).expect("open")
    }

    fn set_config(&self, value: Option<&str>) {
        match value {
            Some(value) => {
                git(self.root(), &["config", "merge.directoryRenames", value]);
            }
            None => {
                let _ = std::process::Command::new("git")
                    .current_dir(self.root())
                    .args(["config", "--unset", "merge.directoryRenames"])
                    .status();
            }
        }
    }
}

fn write(root: &Path, path: &str, tag: &str, lines: usize) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    let body: String = (1..=lines).map(|n| format!("{tag} {n}\n")).collect();
    fs::write(full, body).unwrap();
}

fn mv(root: &Path, from: &str, to: &str) {
    if let Some(parent) = Path::new(to).parent() {
        fs::create_dir_all(root.join(parent)).unwrap();
    }
    git(root, &["mv", from, to]);
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", message]);
}

fn edit_line(root: &Path, path: &str, from: &str, to: &str) {
    let full = root.join(path);
    let text = fs::read_to_string(&full).unwrap();
    fs::write(&full, text.replacen(from, to, 1)).unwrap();
}

fn change_destination(root: &Path, change: DestinationChange, side: &str) {
    match change {
        DestinationChange::Keep => {}
        DestinationChange::Delete => {
            git(root, &["rm", "-q", "b/new"]);
        }
        DestinationChange::Edit => edit_line(root, "b/new", "b/new 1\n", &format!("{side} 1\n")),
    }
}

fn fixture(scenario: Scenario) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    let base_files: &[&str] = match scenario {
        Scenario::Plurality => &["a/w", "a/x", "a/y", "a/z"],
        Scenario::Collision => &["a/x", "a/y", "c/c"],
        Scenario::Nested => &["p/a/x", "p/a/y", "p/q"],
        Scenario::ToRoot => &["a/x", "a/y"],
        Scenario::Destination(..) => &["a/x", "a/y", "b/new"],
        _ => &["a/x", "a/y", "a/z", "keep"],
    };
    for path in base_files {
        write(root, path, path, 20);
    }
    if matches!(
        scenario,
        Scenario::RenamedInto | Scenario::RenamedIntoEdited
    ) {
        write(root, "q/f", "q", 30);
    }
    commit(root, "base");
    git(root, &["branch", "theirs"]);

    // `main` is ours.
    match scenario {
        Scenario::Basic
        | Scenario::Blocked
        | Scenario::RenamedInto
        | Scenario::RenamedIntoEdited
        | Scenario::NewSubdirectoryOnly => mv(root, "a", "b"),
        Scenario::DestinationAddedByRenamer => {
            mv(root, "a", "b");
            write(root, "b/new", "ours-new", 9);
        }
        Scenario::Reverse => write(root, "a/new", "new", 5),
        Scenario::Split => {
            mv(root, "a/x", "b/x");
            mv(root, "a/y", "c/y");
            git(root, &["rm", "-q", "a/z"]);
        }
        Scenario::Plurality => {
            mv(root, "a/x", "b/x");
            mv(root, "a/y", "b/y");
            mv(root, "a/z", "c/z");
            mv(root, "a/w", "d/w");
        }
        Scenario::Partial => {
            mv(root, "a/x", "b/x");
            mv(root, "a/y", "b/y");
        }
        Scenario::Collision => {
            mv(root, "a", "b");
            mv(root, "c/c", "b/c");
        }
        Scenario::Nested => mv(root, "p", "r"),
        Scenario::Destination(ours, _) => {
            change_destination(root, ours, "ours");
            mv(root, "a/x", "b/x");
            mv(root, "a/y", "b/y");
        }
        Scenario::ToRoot => {
            mv(root, "a/x", "x");
            mv(root, "a/y", "y");
        }
    }
    if matches!(scenario, Scenario::RenamedIntoEdited) {
        edit_line(root, "q/f", "q 3\n", "ours 3\n");
    }
    commit(root, "ours");

    git(root, &["switch", "-q", "theirs"]);
    match scenario {
        Scenario::Reverse => mv(root, "a", "b"),
        Scenario::Blocked => {
            write(root, "a/new", "new", 5);
            write(root, "b/new", "other", 5);
        }
        Scenario::Collision => {
            write(root, "a/n", "an", 5);
            write(root, "c/n", "cn", 5);
        }
        Scenario::Nested => {
            write(root, "p/new", "pnew", 5);
            write(root, "p/a/new", "panew", 5);
        }
        Scenario::RenamedInto => mv(root, "q/f", "a/f"),
        Scenario::RenamedIntoEdited => {
            mv(root, "q/f", "a/f");
            edit_line(root, "a/f", "q 20\n", "theirs 20\n");
        }
        Scenario::NewSubdirectoryOnly => write(root, "a/d/e/n", "deep", 5),
        Scenario::Destination(_, theirs) => {
            change_destination(root, theirs, "theirs");
            write(root, "a/new", "new", 5);
        }
        _ => write(root, "a/new", "new", 5),
    }
    commit(root, "theirs");
    git(root, &["switch", "-q", "main"]);

    let rev = |spec: &str| -> ObjectId { git(root, &["rev-parse", spec]).trim().parse().unwrap() };
    Fixture {
        base: rev("main~1"),
        ours: rev("main"),
        theirs: rev("theirs"),
        _tmp: tmp,
    }
}

/// Tree merge result, unmerged stages and conflicted-ness match `git merge-tree` for one
/// `merge.directoryRenames` value.
fn assert_matches_git(fx: &Fixture, config: Option<&str>, context: &str) {
    fx.set_config(config);
    let repo = fx.repo();
    let mode = DirectoryRenameMode::from_repo(&repo).expect("mode");
    let labels = CommitMergeLabels::from_commits(&fx.ours, &fx.theirs);
    let merged = merge_trees_three_way(
        &repo,
        commit_tree(&repo, &fx.base).unwrap(),
        commit_tree(&repo, &fx.ours).unwrap(),
        commit_tree(&repo, &fx.theirs).unwrap(),
        MergeFavor::None,
        Default::default(),
        None,
        labels.presentation(),
        mode,
    )
    .expect("merge trees");
    let oracle = git_merge_tree_oracle(
        fx.root(),
        Some(&fx.base),
        &fx.ours,
        &fx.theirs,
        MergeFavor::None,
    );
    assert_merge_output_matches_oracle(&repo, &merged, &oracle, context);
    assert_eq!(
        tree_merge_has_conflicts(&merged),
        oracle.stdout.contains("CONFLICT ("),
        "{context}: conflicted-ness differs from git:\n{}\ngrit: {:?}",
        oracle.stdout,
        merged.directory_rename_conflicts
    );
}

#[test]
fn tree_merge_matches_git_for_every_scenario_and_config_value() {
    for scenario in SCENARIOS {
        let fx = fixture(scenario);
        for config in [None, Some("conflict"), Some("true"), Some("false")] {
            assert_matches_git(&fx, config, &format!("{scenario:?} with {config:?}"));
        }
    }
}

#[test]
fn reported_conflicts_name_the_directory_rename() {
    let cases: [(Scenario, DirectoryRenameMode, Vec<DirectoryRenameConflict>); 5] = [
        (
            Scenario::Basic,
            DirectoryRenameMode::Conflict,
            vec![DirectoryRenameConflict::Moved {
                from: b"a/new".to_vec(),
                to: b"b/new".to_vec(),
                added_by: MergeSide::Theirs,
            }],
        ),
        (
            Scenario::Reverse,
            DirectoryRenameMode::Conflict,
            vec![DirectoryRenameConflict::Moved {
                from: b"a/new".to_vec(),
                to: b"b/new".to_vec(),
                added_by: MergeSide::Ours,
            }],
        ),
        (
            Scenario::Split,
            DirectoryRenameMode::Apply,
            vec![DirectoryRenameConflict::Split {
                dir: b"a".to_vec(),
                renamed_by: MergeSide::Ours,
            }],
        ),
        (
            Scenario::Blocked,
            DirectoryRenameMode::Conflict,
            vec![DirectoryRenameConflict::Blocked {
                path: b"b/new".to_vec(),
                sources: vec![b"a/new".to_vec()],
                existing: true,
            }],
        ),
        (
            Scenario::Collision,
            DirectoryRenameMode::Apply,
            vec![DirectoryRenameConflict::Blocked {
                path: b"b/n".to_vec(),
                sources: vec![b"a/n".to_vec(), b"c/n".to_vec()],
                existing: false,
            }],
        ),
    ];
    for (scenario, mode, expected) in cases {
        let fx = fixture(scenario);
        let repo = fx.repo();
        let merged = merge_trees_three_way(
            &repo,
            commit_tree(&repo, &fx.base).unwrap(),
            commit_tree(&repo, &fx.ours).unwrap(),
            commit_tree(&repo, &fx.theirs).unwrap(),
            MergeFavor::None,
            Default::default(),
            None,
            Default::default(),
            mode,
        )
        .expect("merge trees");
        assert_eq!(merged.directory_rename_conflicts, expected, "{scenario:?}");
    }
}

fn moved_conflict() -> Conflict {
    Conflict::new(
        "b/new",
        ConflictKind::DirectoryRenameMoved(Box::new(MovedPath {
            original_path: "a/new".into(),
        })),
    )
}

#[test]
fn write_merge_tree_and_predict_merge_follow_the_config() {
    let fx = fixture(Scenario::Basic);
    let oracle = git_merge_tree_oracle(fx.root(), None, &fx.ours, &fx.theirs, MergeFavor::None);
    let repo = fx.repo();
    let options = MergePredictionOptions::default();

    let written = write_merge_tree(&repo, fx.ours, fx.theirs, &options).expect("write");
    assert_eq!(written.tree, oracle.result_tree);
    assert!(!written.is_clean());
    assert_eq!(written.conflicts, vec![moved_conflict()]);
    let new_blob: ObjectId = git(fx.root(), &["rev-parse", "theirs:a/new"])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        written.stages,
        vec![MergeTreeStage {
            mode: 0o100644,
            oid: new_blob,
            stage: 3,
            path: b"b/new".to_vec(),
        }]
    );
    assert_eq!(
        predict_merge(&repo, fx.ours, fx.theirs, &options).expect("predict"),
        MergePrediction::Conflicts {
            conflicts: vec![moved_conflict()]
        }
    );

    fx.set_config(Some("true"));
    let repo = fx.repo();
    let applied = write_merge_tree(&repo, fx.ours, fx.theirs, &options).expect("write");
    assert!(applied.is_clean());
    assert_eq!(applied.tree, oracle.result_tree);
    assert_eq!(
        predict_merge(&repo, fx.ours, fx.theirs, &options).expect("predict"),
        MergePrediction::Clean {
            tree: oracle.result_tree
        }
    );

    let off = MergePredictionOptions {
        directory_renames: Some(DirectoryRenameMode::Off),
        ..MergePredictionOptions::default()
    };
    fx.set_config(Some("false"));
    let off_oracle = git_merge_tree_oracle(fx.root(), None, &fx.ours, &fx.theirs, MergeFavor::None);
    fx.set_config(Some("true"));
    let kept = write_merge_tree(&fx.repo(), fx.ours, fx.theirs, &off).expect("write");
    assert!(kept.is_clean());
    assert_eq!(kept.tree, off_oracle.result_tree);
    assert_ne!(kept.tree, oracle.result_tree);
}

/// `plan_branch_merge` (dry-run) honors an explicit `MergeOptions::directory_renames` over the
/// repository config, like `integrate_branch_with` does.
#[test]
fn dry_run_plan_follows_the_explicit_directory_rename_option() {
    let fx = fixture(Scenario::Basic);
    let plan = |config: &str, mode: Option<DirectoryRenameMode>| {
        fx.set_config(Some(config));
        let options = MergeOptions::default().with_directory_renames(mode);
        plan_branch_merge(&fx.repo(), fx.ours, fx.theirs, "theirs", &options).expect("plan")
    };
    let merge_commit = MergeDryRunPlan::MergeCommit {
        parents: vec![fx.ours, fx.theirs],
    };
    let moved = MergeDryRunPlan::Conflicts {
        conflicts: vec![moved_conflict()],
    };

    assert_eq!(plan("true", None), merge_commit);
    assert_eq!(plan("true", Some(DirectoryRenameMode::Conflict)), moved);
    assert_eq!(plan("conflict", None), moved);
    assert_eq!(
        plan("conflict", Some(DirectoryRenameMode::Apply)),
        merge_commit
    );
    assert_eq!(
        plan("conflict", Some(DirectoryRenameMode::Off)),
        merge_commit
    );
}

#[test]
fn integrate_branch_stops_and_git_reads_the_result() {
    let fx = fixture(Scenario::Basic);
    let root = fx.root();
    let oracle = git_merge_tree_oracle(root, None, &fx.ours, &fx.theirs, MergeFavor::None);
    let repo = fx.repo();
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        fx.ours,
        fx.theirs,
        "theirs",
        &MergeOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("merge");
    let IntegrateBranchOutcome::Conflicts { paths, details, .. } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(paths, vec!["b/new"]);
    assert_eq!(details, vec![moved_conflict()]);
    assert_eq!(git(root, &["rev-parse", "main"]).trim(), fx.ours.to_hex());

    let rows: Vec<(String, u32)> = git_ls_files_stage(root)
        .into_iter()
        .filter(|(_, stage, ..)| *stage != 0)
        .map(|(path, stage, ..)| (path, stage))
        .collect();
    assert_eq!(rows, vec![("b/new".into(), 3)]);
    assert_eq!(
        fs::read_to_string(root.join("b/new")).unwrap(),
        git(root, &["cat-file", "-p", "theirs:a/new"])
    );
    assert!(!root.join("a").exists(), "a/ must not come back");
    assert!(git(root, &["status", "--porcelain"]).contains("UA b/new"));

    git(root, &["add", "b/new"]);
    git(root, &["commit", "-qm", "merged"]);
    assert_eq!(
        git(root, &["rev-parse", "HEAD^{tree}"]).trim(),
        oracle.result_tree.to_hex()
    );
    assert_git_fsck_strict(root);
}

#[test]
fn split_directory_stops_without_unmerged_paths() {
    let fx = fixture(Scenario::Split);
    let root = fx.root();
    let repo = fx.repo();
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        fx.ours,
        fx.theirs,
        "theirs",
        &MergeOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("merge");
    let IntegrateBranchOutcome::Conflicts { details, .. } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(
        details,
        vec![Conflict::new("a", ConflictKind::DirectoryRenameSplit)]
    );
    assert!(root.join(".git/MERGE_HEAD").exists());
    assert!(root.join("a/new").exists());
    assert!(git_ls_files_stage(root)
        .iter()
        .all(|(_, stage, ..)| *stage == 0));
    assert_git_fsck_strict(root);
}

#[test]
fn grit_reads_the_index_git_leaves_after_a_directory_rename_conflict() {
    let fx = fixture(Scenario::Basic);
    let root = fx.root();
    let merge = std::process::Command::new("git")
        .current_dir(root)
        .args(["merge", "-q", "--no-edit", "theirs"])
        .output()
        .expect("git merge");
    assert!(!merge.status.success(), "git must stop on the moved file");
    let repo = fx.repo();
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    assert_eq!(
        conflicts_from_index(&index),
        vec![Conflict::new("b/new", ConflictKind::AddedByThem)]
    );
}

#[test]
fn pick_of_the_added_file_follows_the_rename() {
    let fx = fixture(Scenario::Basic);
    let outcome = replay_commit(
        &fx.repo(),
        &ReplayRequest {
            commit: fx.theirs,
            direction: ReplayDirection::Pick,
            committer: COMMITTER.parse().expect("COMMITTER"),
            message_override: None,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("pick");
    let ReplayOutcome::Conflicts { paths } = outcome else {
        panic!("expected conflicts, got {outcome:?}");
    };
    assert_eq!(paths, vec!["b/new"]);
}

#[test]
fn moved_file_conflicts_round_trip_through_conflicts_from_tree_merge() {
    let fx = fixture(Scenario::RenamedIntoEdited);
    let repo = fx.repo();
    let merged = merge_trees_three_way(
        &repo,
        commit_tree(&repo, &fx.base).unwrap(),
        commit_tree(&repo, &fx.ours).unwrap(),
        commit_tree(&repo, &fx.theirs).unwrap(),
        MergeFavor::None,
        Default::default(),
        None,
        Default::default(),
        DirectoryRenameMode::Conflict,
    )
    .expect("merge trees");
    assert_eq!(
        conflicts_from_tree_merge(&merged),
        vec![Conflict::new(
            "b/f",
            ConflictKind::DirectoryRenameMoved(Box::new(MovedPath {
                original_path: "a/f".into(),
            }))
        )]
    );
}

/// `grit merge --abort` must delete files that only exist as conflict stages (the moved file
/// here, or the modified side of a modify/delete), as `git merge --abort` does.
#[test]
fn abort_removes_files_that_only_exist_as_conflict_stages() {
    let fx = fixture(Scenario::Basic);
    let root = fx.root();
    let repo = fx.repo();
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        fx.ours,
        fx.theirs,
        "theirs",
        &MergeOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(root.join("b/new").exists());
    abort_branch_merge(&repo).expect("abort");
    assert!(!root.join("b/new").exists());
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    write(root, "d/f", "f", 10);
    commit(root, "base");
    git(root, &["switch", "-qc", "topic"]);
    edit_line(root, "d/f", "f 1\n", "topic 1\n");
    commit(root, "edit");
    git(root, &["switch", "-q", "main"]);
    git(root, &["rm", "-q", "d/f"]);
    commit(root, "delete");
    let rev = |spec: &str| -> ObjectId { git(root, &["rev-parse", spec]).trim().parse().unwrap() };
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        rev("main"),
        rev("topic"),
        "topic",
        &MergeOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(root.join("d/f").exists());
    abort_branch_merge(&repo).expect("abort");
    assert!(!root.join("d").exists());
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

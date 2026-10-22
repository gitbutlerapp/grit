//! Switch overwrite errors match system `git switch` refusal sets.

mod switch_overwrite_git_helpers;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, OverwriteReason};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

use switch_overwrite_git_helpers::{
    dir_bytes_snapshot, git_reflogs_snapshot, git_switch_refused_paths, worktree_file_snapshot,
};

fn init_repo_with_two_branches(
    tmp: &tempfile::TempDir,
) -> (Repository, String, ObjectId, String, ObjectId) {
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    std::fs::write(tmp.path().join("shared.txt"), "base\n").expect("write");
    std::fs::write(tmp.path().join("only-main.txt"), "main\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["switch", "-q", "-c", "other"]);
    std::fs::write(tmp.path().join("shared.txt"), "other\n").expect("write");
    std::fs::write(tmp.path().join("only-other.txt"), "other-only\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "other"]);
    let other_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "other"]).trim()).expect("other oid");

    git(tmp.path(), &["switch", "-q", "main"]);
    let main_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim()).expect("main oid");

    let repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");

    (repo, "main".into(), main_oid, "other".into(), other_oid)
}

#[test]
fn switch_local_changes_refusal_paths_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, _main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    std::fs::write(tmp.path().join("shared.txt"), "local edit\n").expect("edit");

    let git_paths = git_switch_refused_paths(tmp.path(), &other);

    let err = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other.clone(),
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect_err("switch");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => {
            assert_eq!(paths, git_paths);
            assert!(paths.contains(&"shared.txt".to_owned()));
        }
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
}

#[test]
fn switch_untracked_refusal_paths_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, _main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    std::fs::write(tmp.path().join("only-other.txt"), "untracked copy\n").expect("untracked");

    let git_paths = git_switch_refused_paths(tmp.path(), &other);

    let err = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other,
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect_err("switch");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::Untracked,
            paths,
            ..
        } => {
            assert_eq!(paths, git_paths);
            assert!(paths.contains(&"only-other.txt".to_owned()));
        }
        other => panic!("expected WouldOverwrite Untracked, got {other:?}"),
    }
}

#[test]
fn switch_dry_run_leaves_repo_unchanged_and_reports_path_count() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _main, main_oid, other, other_oid) = init_repo_with_two_branches(&tmp);

    let index_before = dir_bytes_snapshot(tmp.path(), ".git/index");
    let head_before = dir_bytes_snapshot(tmp.path(), ".git/HEAD");
    let reflogs_before = git_reflogs_snapshot(tmp.path().join(".git").as_path());
    let worktree_before = worktree_file_snapshot(tmp.path());

    let git_count = git(tmp.path(), &["diff", "--name-only", "HEAD", "other"])
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other.clone(),
                oid: other_oid,
            },
            dry_run: true,
        },
    )
    .expect("dry-run switch");
    assert!(outcome.dry_run);
    assert_eq!(outcome.changed_paths, git_count);
    assert_eq!(outcome.commit, other_oid);

    assert_eq!(dir_bytes_snapshot(tmp.path(), ".git/index"), index_before);
    assert_eq!(dir_bytes_snapshot(tmp.path(), ".git/HEAD"), head_before);
    assert_eq!(
        git_reflogs_snapshot(tmp.path().join(".git").as_path()),
        reflogs_before
    );
    assert_eq!(worktree_file_snapshot(tmp.path()), worktree_before);

    // Sanity: a real switch still works afterward.
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: other,
                oid: other_oid,
            },
            dry_run: false,
        },
    )
    .expect("real switch");
    let _ = main_oid;
}

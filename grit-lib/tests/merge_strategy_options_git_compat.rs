//! Merge strategy options (`-X`, conflict style, unrelated histories) vs system `git`.

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::{ConflictStyle, MergeFavor};
use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::merge::{
    integrate_branch_with, resolve_merge_conflict_style, IntegrateBranchOutcome, MergeCommitMode,
    MergeOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::fs;
use std::process::Command;

use support::{
    assert_merge_commits_match_git_with_favor, open_grit_repo, CommitMergeLabels, HistoryRepo,
};

fn grit_repo(tmp: &tempfile::TempDir) -> Repository {
    Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open grit repo")
}

fn setup_identities(path: &std::path::Path) {
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "Test"]);
}

fn favor_conflict_repo() -> (tempfile::TempDir, ObjectId, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    fs::write(tmp.path().join("f.txt"), "a\nb\nc\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let base: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("base");
    git(tmp.path(), &["switch", "-c", "topic"]);
    fs::write(tmp.path().join("f.txt"), "a\nTHEIRS\nc\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "theirs"]);
    let theirs: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("theirs");
    git(tmp.path(), &["switch", "main"]);
    fs::write(tmp.path().join("f.txt"), "a\nOURS\nc\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "ours"]);
    let ours: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("ours");
    (tmp, base, ours, theirs)
}

fn marker_conflict_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    fs::write(tmp.path().join("f.txt"), "line1\nline2\nline3\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-qm", "base"]);
    git(tmp.path(), &["branch", "side"]);
    git(tmp.path(), &["switch", "side"]);
    fs::write(tmp.path().join("f.txt"), "line1\nside\nline3\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-qm", "side"]);
    let side: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("side");
    git(tmp.path(), &["switch", "main"]);
    fs::write(tmp.path().join("f.txt"), "line1\nmaster\nline3\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-qm", "master"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    (tmp, main, side)
}

fn git_merge_expect_conflict(dir: &tempfile::TempDir, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge");
    assert!(
        !out.status.success(),
        "merge should conflict: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn worktree_file(dir: &tempfile::TempDir) -> Vec<u8> {
    fs::read(dir.path().join("f.txt")).unwrap_or_default()
}

#[test]
#[allow(non_snake_case)]
fn x_ours_matches_git_merge_X_ours_tree() {
    let (tmp, base, ours, theirs) = favor_conflict_repo();
    let history = HistoryRepo::adopt_temp(tmp);
    let grit = open_grit_repo(history.worktree());
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git_with_favor(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        MergeFavor::Ours,
        "merge -X ours tree",
    );
}

#[test]
fn x_theirs_matches_git_tree() {
    let (tmp, base, ours, theirs) = favor_conflict_repo();
    let history = HistoryRepo::adopt_temp(tmp);
    let grit = open_grit_repo(history.worktree());
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git_with_favor(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        MergeFavor::Theirs,
        "merge -X theirs tree",
    );
}

#[test]
fn diff3_markers_byte_equal_git() {
    let (tmp, _main, _side) = marker_conflict_repo();
    git(tmp.path(), &["config", "merge.conflictStyle", "diff3"]);
    git_merge_expect_conflict(&tmp, &["side"]);

    let (tmp2, main2, side2) = marker_conflict_repo();
    let repo = grit_repo(&tmp2);
    git(tmp2.path(), &["config", "merge.conflictStyle", "diff3"]);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main2,
        side2,
        "side",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            conflict_style: Some(ConflictStyle::Diff3),
            ..MergeOptions::default()
        },
    )
    .expect("grit merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert_eq!(worktree_file(&tmp), worktree_file(&tmp2));
}

#[test]
fn zdiff3_markers_byte_equal_git() {
    let (tmp, _main, _side) = marker_conflict_repo();
    git(tmp.path(), &["config", "merge.conflictStyle", "zdiff3"]);
    git_merge_expect_conflict(&tmp, &["side"]);

    let (tmp2, main2, side2) = marker_conflict_repo();
    let repo = grit_repo(&tmp2);
    git(tmp2.path(), &["config", "merge.conflictStyle", "zdiff3"]);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main2,
        side2,
        "side",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            conflict_style: Some(ConflictStyle::ZealousDiff3),
            ..MergeOptions::default()
        },
    )
    .expect("grit merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert_eq!(worktree_file(&tmp), worktree_file(&tmp2));
}

#[test]
fn conflict_style_from_config() {
    let (tmp, _main, _side) = marker_conflict_repo();
    git(tmp.path(), &["config", "merge.conflictStyle", "diff3"]);
    git_merge_expect_conflict(&tmp, &["side"]);

    let (tmp2, main2, side2) = marker_conflict_repo();
    let repo = grit_repo(&tmp2);
    git(tmp2.path(), &["config", "merge.conflictStyle", "diff3"]);
    assert_eq!(
        resolve_merge_conflict_style(&repo, None),
        ConflictStyle::Diff3
    );
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main2,
        side2,
        "side",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            ..MergeOptions::default()
        },
    )
    .expect("grit merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert_eq!(worktree_file(&tmp), worktree_file(&tmp2));
}

#[test]
fn unrelated_refused_by_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    fs::write(tmp.path().join("a.txt"), "a\n").expect("write");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "main"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    git(tmp.path(), &["switch", "--orphan", "orphan"]);
    fs::write(tmp.path().join("b.txt"), "b\n").expect("write");
    git(tmp.path(), &["add", "b.txt"]);
    git(tmp.path(), &["commit", "-m", "orphan"]);
    let orphan: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("orphan");
    git(tmp.path(), &["switch", "main"]);
    let repo = grit_repo(&tmp);
    let err = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        orphan,
        "orphan",
        &MergeOptions::default(),
    )
    .expect_err("unrelated without flag");
    assert!(matches!(err, Error::Message(_)));
}

#[test]
fn allow_unrelated_matches_git_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    fs::write(tmp.path().join("main.txt"), "main\n").expect("write");
    git(tmp.path(), &["add", "main.txt"]);
    git(tmp.path(), &["commit", "-m", "main"]);
    let _main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    git(tmp.path(), &["switch", "--orphan", "orphan"]);
    fs::write(tmp.path().join("orph.txt"), "orph\n").expect("write");
    git(tmp.path(), &["add", "orph.txt"]);
    git(tmp.path(), &["commit", "-m", "orphan"]);
    let _orphan: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("orphan");
    git(tmp.path(), &["switch", "main"]);

    git(
        tmp.path(),
        &[
            "merge",
            "--allow-unrelated-histories",
            "orphan",
            "-m",
            "merge",
        ],
    );
    let git_tree: ObjectId = git(tmp.path(), &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("git tree");

    let tmp2 = tempfile::tempdir().expect("tempdir2");
    git(tmp2.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp2.path());
    fs::write(tmp2.path().join("main.txt"), "main\n").expect("write");
    git(tmp2.path(), &["add", "main.txt"]);
    git(tmp2.path(), &["commit", "-m", "main"]);
    let main2: ObjectId = git(tmp2.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main2");
    git(tmp2.path(), &["switch", "--orphan", "orphan"]);
    fs::write(tmp2.path().join("orph.txt"), "orph\n").expect("write");
    git(tmp2.path(), &["add", "orph.txt"]);
    git(tmp2.path(), &["commit", "-m", "orphan"]);
    let orphan2: ObjectId = git(tmp2.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("orphan2");
    git(tmp2.path(), &["switch", "main"]);

    let repo = grit_repo(&tmp2);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main2,
        orphan2,
        "orphan",
        &MergeOptions {
            allow_unrelated: true,
            ..MergeOptions::default()
        },
    )
    .expect("allow unrelated");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected merged, got {outcome:?}");
    };
    let obj = repo.odb().read(&oid).expect("commit");
    let grit_tree = parse_commit(&obj.data).expect("parse").tree;

    let oracle = support::git_merge_tree_oracle_with_options(
        tmp2.path(),
        None,
        &main2,
        &orphan2,
        MergeFavor::None,
        true,
    );
    assert_eq!(
        grit_tree, git_tree,
        "integrate tree must match git merge commit tree"
    );
    assert_eq!(
        grit_tree, oracle.result_tree,
        "integrate tree must match git merge-tree oracle"
    );
}

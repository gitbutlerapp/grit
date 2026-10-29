//! Issue #960: grit merge leaves merge-in-progress state with conflict markers.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "Merge Test <merge@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: Some(id.parse().expect("signature")),
        committer: Some(id.parse().expect("signature")),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    git(dir, args).trim().to_owned()
}

fn issue_960_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    let base: String = (1..=12)
        .map(|i| format!("a line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(root.join("a.txt"), format!("{base}\n")).expect("a.txt");
    fs::write(root.join("b.txt"), "b base\n").expect("b.txt");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-c", "feature-x"]);
    let mut feature_a = base.lines().collect::<Vec<_>>();
    feature_a[4] = "FEATURE";
    feature_a[5] = "FEATURE";
    fs::write(root.join("a.txt"), format!("{}\n", feature_a.join("\n"))).expect("feature a");
    fs::write(root.join("x.txt"), "new on feature\n").expect("x.txt");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "feature"]);

    git(root, &["switch", "main"]);
    let mut main_a = base.lines().collect::<Vec<_>>();
    main_a[4] = "MAIN";
    main_a[5] = "MAIN";
    fs::write(root.join("a.txt"), format!("{}\n", main_a.join("\n"))).expect("main a");
    fs::write(root.join("b.txt"), "b on main\n").expect("b main");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "main"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo");
    (tmp, repo)
}

#[test]
fn grit_merge_leaves_conflict_markers_and_clean_side_files() {
    let (_tmp, repo) = issue_960_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");
    let feature_oid =
        ObjectId::from_hex(&git_out(root, &["rev-parse", "feature-x"])).expect("feature");

    let git_tree_out =
        grit_test_support::git_cmd(&["merge-tree", "--write-tree", "main", "feature-x"])
            .in_dir(root)
            .exec();
    let git_tree = git_tree_out
        .stdout
        .lines()
        .next()
        .expect("merge-tree tree oid")
        .trim();

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        feature_oid,
        "feature-x",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }),
        "expected conflicts, got {outcome:?}"
    );

    assert!(repo.git_dir().join("MERGE_HEAD").exists());
    assert!(repo.git_dir().join("ORIG_HEAD").exists());
    assert!(repo.git_dir().join("MERGE_MSG").exists());

    let index = repo.index_handle().load().expect("index");
    assert!(index.has_unmerged_entries());

    let a_body = fs::read_to_string(root.join("a.txt")).expect("a.txt");
    assert!(
        a_body.contains("<<<<<<<"),
        "expected conflict markers in a.txt:\n{a_body}"
    );
    assert!(
        a_body.contains(">>>>>>>"),
        "expected conflict markers in a.txt:\n{a_body}"
    );
    assert!(
        a_body.contains("MAIN"),
        "ours side should appear in markers"
    );
    assert!(
        a_body.contains("FEATURE"),
        "theirs side should appear in markers"
    );
    assert!(
        a_body.matches("<<<<<<<").count() == 1,
        "only the overlapping hunk should conflict:\n{a_body}"
    );

    assert_eq!(
        fs::read_to_string(root.join("x.txt")).expect("x.txt"),
        "new on feature\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.txt")).expect("b.txt"),
        "b on main\n"
    );

    let grit_stage0 = grit_lib::index::write_tree::write_tree_from_index(repo.odb(), &index, "")
        .expect("stage0 tree");
    for path in ["b.txt", "x.txt"] {
        let grit_oid = git_out(root, &["ls-tree", "-r", &grit_stage0.to_hex(), "--", path]);
        let git_oid = git_out(root, &["ls-tree", "-r", git_tree, "--", path]);
        assert_eq!(
            grit_oid, git_oid,
            "merged blob for {path} should match git merge-tree"
        );
    }
    assert!(
        git_out(
            root,
            &["ls-tree", "-r", &grit_stage0.to_hex(), "--", "a.txt"]
        )
        .is_empty(),
        "conflicted path should stay out of stage-0 tree"
    );

    let fsck = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn resolve_and_commit_matches_git_merge() {
    let (_tmp, repo) = issue_960_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");
    let feature_oid =
        ObjectId::from_hex(&git_out(root, &["rev-parse", "feature-x"])).expect("feature");

    integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        feature_oid,
        "feature-x",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");

    let mut resolved = fs::read_to_string(root.join("a.txt")).expect("a.txt");
    resolved = resolved
        .lines()
        .filter(|line| {
            !line.starts_with("<<<<<<<")
                && !line.starts_with("=======")
                && !line.starts_with(">>>>>>>")
                && !line.starts_with("|||||||")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(root.join("a.txt"), format!("{resolved}\n")).expect("write resolved");

    git(root, &["add", "a.txt"]);
    let outcome = create_commit(
        &repo,
        &commit_req("Merge feature-x"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge commit");

    let side_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "feature-x"])).expect("side");
    let commit_obj = repo.odb().read(&outcome.oid).expect("commit");
    let parsed = parse_commit(&commit_obj.data).expect("parse");
    assert_eq!(parsed.parents, vec![main_oid, side_tip]);
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
}

fn issue_1090_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("a.txt");
    fs::write(root.join("b.txt"), "hi\n").expect("b.txt");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "side"]);
    fs::write(root.join("a.txt"), "one\nside\nthree\n").expect("side a");
    git(root, &["commit", "-qam", "side"]);

    git(root, &["switch", "-q", "main"]);
    fs::write(root.join("a.txt"), "one\nmain\nthree\n").expect("main a");
    git(root, &["commit", "-qam", "main"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo");
    (tmp, repo)
}

/// Issue #1090: non-conflicting paths must stay in index and worktree during conflict merge.
#[test]
fn grit_merge_keeps_identical_non_conflict_files() {
    let (_tmp, repo) = issue_1090_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");
    let side_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side");

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        side_oid,
        "side",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }),
        "expected conflicts, got {outcome:?}"
    );

    assert!(
        root.join("b.txt").is_file(),
        "b.txt must remain in the worktree during conflict merge"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.txt")).expect("b.txt"),
        "hi\n"
    );

    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"b.txt", 0).is_some(),
        "b.txt must stay at stage 0 in the index"
    );
    assert!(index.get(b"a.txt", 0).is_none());
    assert!(index.get(b"a.txt", 1).is_some());
    assert!(index.get(b"a.txt", 2).is_some());
    assert!(index.get(b"a.txt", 3).is_some());

    let git_status = git_out(root, &["status", "-s"]);
    assert!(
        git_status.contains("UU a.txt"),
        "git status should show unmerged a.txt:\n{git_status}"
    );
    assert!(
        !git_status.contains("b.txt"),
        "git status must not show b.txt as deleted:\n{git_status}"
    );

    let grit_ls = git_out(root, &["ls-files", "-s"]);
    assert!(
        grit_ls
            .lines()
            .any(|l| l.contains("\tb.txt") && l.split_whitespace().nth(2) == Some("0")),
        "git ls-files must list b.txt at stage 0:\n{grit_ls}"
    );
}

#[test]
fn issue_1090_resolve_commit_keeps_non_conflict_files() {
    let (_tmp, repo) = issue_1090_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");
    let side_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side");

    integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        side_oid,
        "side",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");

    fs::write(root.join("a.txt"), "resolved\n").expect("resolve a.txt");
    git(root, &["add", "a.txt"]);
    let outcome = create_commit(
        &repo,
        &commit_req("resolved"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge commit");

    let tree_paths = git_out(
        root,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &format!("{}^{{tree}}", outcome.oid.to_hex()),
        ],
    );
    assert!(
        tree_paths.lines().any(|p| p == "a.txt"),
        "merge commit tree must contain a.txt"
    );
    assert!(
        tree_paths.lines().any(|p| p == "b.txt"),
        "merge commit tree must still contain b.txt (issue #1090)"
    );

    let git_merge_dir = tempfile::tempdir().expect("git merge dir");
    let git_root = git_merge_dir.path();
    git(git_root, &["init", "-q", "-b", "main"]);
    git(git_root, &["config", "user.name", "T"]);
    git(git_root, &["config", "user.email", "t@e.com"]);
    fs::write(git_root.join("a.txt"), "one\ntwo\nthree\n").expect("a.txt");
    fs::write(git_root.join("b.txt"), "hi\n").expect("b.txt");
    git(git_root, &["add", "-A"]);
    git(git_root, &["commit", "-qm", "base"]);
    git(git_root, &["switch", "-q", "-c", "side"]);
    fs::write(git_root.join("a.txt"), "one\nside\nthree\n").expect("side a");
    git(git_root, &["commit", "-qam", "side"]);
    git(git_root, &["switch", "-q", "main"]);
    fs::write(git_root.join("a.txt"), "one\nmain\nthree\n").expect("main a");
    git(git_root, &["commit", "-qam", "main"]);
    let merge_out = grit_test_support::git_cmd(&["merge", "side"])
        .in_dir(git_root)
        .exec();
    assert!(
        !merge_out.ok(),
        "git merge should stop on conflict: {}",
        merge_out.stderr
    );
    fs::write(git_root.join("a.txt"), "resolved\n").expect("resolve");
    git(git_root, &["add", "a.txt"]);
    git(git_root, &["commit", "-qm", "resolved"]);

    let grit_tree = git_out(
        root,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &format!("{}^{{tree}}", outcome.oid.to_hex()),
        ],
    );
    let git_tree = git_out(git_root, &["ls-tree", "-r", "--name-only", "HEAD^{tree}"]);
    assert_eq!(
        grit_tree, git_tree,
        "merge commit tree should match system git"
    );
}

#[test]
fn merge_abort_restores_pre_merge_state() {
    let (_tmp, repo) = issue_960_repo();
    let root = repo.work_tree().expect("worktree");

    let main_before = git_out(root, &["rev-parse", "HEAD"]);
    let main_tree_before = git_out(root, &["rev-parse", "HEAD^{tree}"]);
    let a_before = fs::read_to_string(root.join("a.txt")).expect("a before");
    let b_before = fs::read_to_string(root.join("b.txt")).expect("b before");

    let main_oid = ObjectId::from_hex(&main_before).expect("main");
    let feature_oid =
        ObjectId::from_hex(&git_out(root, &["rev-parse", "feature-x"])).expect("feature");

    integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        feature_oid,
        "feature-x",
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");

    abort_branch_merge(&repo).expect("abort");

    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    assert_eq!(git_out(root, &["rev-parse", "HEAD"]), main_before);
    assert_eq!(
        git_out(root, &["rev-parse", "HEAD^{tree}"]),
        main_tree_before
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).expect("a"), a_before);
    assert_eq!(fs::read_to_string(root.join("b.txt")).expect("b"), b_before);
    assert!(!root.join("x.txt").exists());
    let index = repo.index_handle().load().expect("index after abort");
    let b_index = index.get(b"b.txt", 0).expect("b.txt tracked");
    assert!(
        !b_index.stat_cache_unset(),
        "unchanged paths must keep seeded stat data after merge abort"
    );
}

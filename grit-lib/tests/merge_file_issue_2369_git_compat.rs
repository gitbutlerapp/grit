//! Issue #2369: adjacent delete vs insert must conflict like `git merge`.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{
    integrate_branch, predict_merge, IntegrateBranchOutcome, MergePrediction,
    MergePredictionOptions,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

#[test]
fn issue_2369_repo_merge_and_predict_match_git_merge_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "a@b"]);
    git(root, &["config", "user.name", "A"]);

    let content: String = (1..=10).map(|i| format!("{i}\n")).collect();
    fs::write(root.join("f"), &content).expect("f");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    let base_oid =
        ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim().to_owned()).expect("base");

    git(root, &["switch", "-c", "theirs"]);
    let mut lines: Vec<_> = content.lines().collect();
    lines.insert(5, "NEW1");
    lines.insert(6, "NEW2");
    fs::write(root.join("f"), format!("{}\n", lines.join("\n"))).expect("theirs f");
    git(root, &["commit", "-qam", "theirs"]);
    let theirs_oid =
        ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim().to_owned()).expect("theirs");

    git(root, &["switch", "main"]);
    let mut main_lines: Vec<_> = content.lines().collect();
    main_lines.drain(2..5);
    fs::write(root.join("f"), format!("{}\n", main_lines.join("\n"))).expect("ours f");
    git(root, &["commit", "-qam", "ours"]);
    let main_oid =
        ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim().to_owned()).expect("main");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
    let prediction = predict_merge(
        &repo,
        main_oid,
        theirs_oid,
        &MergePredictionOptions::default(),
    )
    .expect("predict_merge");
    assert!(
        matches!(prediction, MergePrediction::Conflicts { .. }),
        "predict_merge must report a content conflict"
    );

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main_oid,
        theirs_oid,
        "theirs",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("integrate");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }),
        "grit merge must stop with conflicts"
    );

    let grit_merged = fs::read(root.join("f")).expect("grit merged f");
    let base_file = git(root, &["show", &format!("{base_oid}:f")]);
    let ours_file = format!("{}\n", main_lines.join("\n"));
    let theirs_file = format!("{}\n", lines.join("\n"));

    let git_dir = tempfile::tempdir().expect("git merge dir");
    fs::write(git_dir.path().join("base"), base_file.as_bytes()).expect("base");
    fs::write(git_dir.path().join("ours"), ours_file.as_bytes()).expect("ours");
    fs::write(git_dir.path().join("theirs"), theirs_file.as_bytes()).expect("theirs");
    let git_merged = Command::new("git")
        .current_dir(git_dir.path())
        .args([
            "merge-file",
            "-p",
            "-L",
            "HEAD",
            "-L",
            "base",
            "-L",
            "theirs",
            "ours",
            "base",
            "theirs",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge-file");
    assert_eq!(git_merged.status.code(), Some(1));
    assert_eq!(
        grit_merged, git_merged.stdout,
        "working-tree conflict bytes must match git merge-file"
    );

    let git_status = git(root, &["status", "--short"]);
    assert!(
        git_status.contains("UU"),
        "git status should show unmerged f (UU), got: {git_status}"
    );
}

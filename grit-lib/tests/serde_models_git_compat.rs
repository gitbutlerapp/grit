//! `serde-models` output for repositories built by the system `git`.

#![cfg(feature = "serde-models")]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::log::DecorationMap;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repository")
}

/// A merge `git` paused on a conflict serializes with its operation, conflict kinds and
/// typed diff modes, so a front end can render the paused state without its own DTO.
#[test]
fn status_model_of_git_paused_merge_serializes_operation_and_conflicts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "base\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["switch", "-q", "-c", "side"]);
    std::fs::write(root.join("a.txt"), "side\n").expect("write");
    git(root, &["commit", "-q", "-am", "side"]);
    git(root, &["switch", "-q", "main"]);
    std::fs::write(root.join("a.txt"), "main\n").expect("write");
    git(root, &["commit", "-q", "-am", "main"]);
    let _ = grit_test_support::git_cmd(&["merge", "-q", "side"])
        .in_dir(root)
        .with_status(1);

    let repo = open(root);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("status");
    let json = serde_json::to_value(&model).expect("serialize status");

    assert_eq!(json["head"]["branch"]["short_name"], "main");
    assert_eq!(json["state"]["merge_in_progress"], true);
    assert_eq!(json["operation"]["kind"], "merge");
    assert_eq!(json["conflicts"][0], "a.txt");
    assert_eq!(json["conflict_details"][0]["path"], "a.txt");
    assert_eq!(json["conflict_details"][0]["kind"], "both_modified");
    assert!(
        json.get("index").unwrap().is_none(),
        "the index is not serialized"
    );
}

/// Author and committer dates match what `git log` reports for the same commit.
#[test]
fn loaded_commit_signatures_match_git_log_dates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    grit_test_support::git_cmd(&["commit", "-q", "-m", "first"])
        .in_dir(root)
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 -0800")
        .env("GIT_COMMITTER_DATE", "1700003600 +0530")
        .suc();

    let head = git(root, &["rev-parse", "HEAD"]);
    let oid = grit_lib::objects::ObjectId::from_hex(head.trim()).expect("oid");
    let repo = open(root);
    let loaded = repo
        .revwalk()
        .load_commits(&[oid], &DecorationMap::default(), &mut ())
        .expect("load");
    let json = serde_json::to_value(&loaded[0]).expect("serialize commit");

    let git_dates = git(root, &["log", "-1", "--format=%aI%n%cI"]);
    let mut lines = git_dates.lines();
    assert_eq!(json["author"]["name"], "Ada Lovelace");
    assert_eq!(json["author"]["email"], "ada@example.com");
    assert_eq!(json["author"]["date"], lines.next().expect("author date"));
    assert_eq!(
        json["committer"]["date"],
        lines.next().expect("committer date")
    );
}

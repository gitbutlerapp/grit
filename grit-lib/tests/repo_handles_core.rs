//! Cross-check typed repository handles against system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::diff::{format_raw, DiffEntry};
use grit_lib::objects::ObjectKind;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;
use grit_lib::rev_list::RevListOptions;

mod support;

use support::{
    assert_git_fsck_strict, commit_file, empty_commit_oid, files_repo, git, open_grit_repo,
    HistoryRepo, TestRepo,
};

fn grit_repo_from_test(test: &TestRepo) -> Repository {
    open_grit_repo(test.worktree())
}

fn seeded_files_repo() -> (TestRepo, Repository) {
    let test = files_repo();
    empty_commit_oid(&test);
    let repo = grit_repo_from_test(&test);
    (test, repo)
}

#[test]
fn objects_read_write_contains_match_git_cat_file() {
    let test = files_repo();
    let repo = grit_repo_from_test(&test);
    let wt = test.worktree();

    let oid = repo
        .objects()
        .write(ObjectKind::Blob, b"handle-roundtrip\n")
        .expect("write blob");
    assert!(repo.objects().contains(&oid));

    assert_git_fsck_strict(wt);

    let git_type = git(wt, &["cat-file", "-t", &oid.to_hex()]);
    assert_eq!(git_type.trim(), "blob");

    let git_payload = git(wt, &["cat-file", "-p", &oid.to_hex()]);
    assert_eq!(git_payload, "handle-roundtrip\n");

    let obj = repo.objects().read(&oid).expect("read back");
    assert_eq!(obj.data, b"handle-roundtrip\n");
}

#[test]
fn references_update_roundtrip_fsck_with_git() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    let commit = empty_commit_oid(&test);
    let refname = RefName::try_from("refs/heads/handle-branch").expect("valid ref");
    repo.references()
        .update(&refname, &commit)
        .expect("update ref");

    assert_git_fsck_strict(wt);

    let git_oid = git(wt, &["rev-parse", "refs/heads/handle-branch"])
        .trim()
        .to_owned();
    assert_eq!(git_oid, commit.to_hex());

    assert_eq!(
        repo.references().resolve(&refname).expect("resolve"),
        commit
    );
}

#[test]
fn references_list_matches_git_for_each_ref() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    let listed = repo.references().list("refs/heads/").expect("list heads");
    assert!(listed.iter().any(|(n, _)| n == "refs/heads/main"));

    let git_out = git(wt, &["for-each-ref", "--format=%(refname)", "refs/heads/"]);
    let mut git_names: Vec<_> = git_out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    git_names.sort();
    let mut grit_names: Vec<_> = listed.into_iter().map(|(n, _)| n).collect();
    grit_names.sort();
    assert_eq!(grit_names, git_names);
}

#[test]
fn index_handle_load_write_matches_git_ls_files() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    fs::write(wt.join("tracked.txt"), "index handle\n").expect("write file");
    git(wt, &["add", "tracked.txt"]);

    let mut idx = repo.index_handle().load().expect("load index");
    assert!(idx
        .entries()
        .iter()
        .any(|e| e.path.as_slice() == b"tracked.txt"));
    repo.index_handle().write(&mut idx).expect("write index");

    let git_stages = git(wt, &["ls-files", "-s"]);
    assert!(git_stages.contains("tracked.txt"));
}

#[test]
fn config_handle_get_set_matches_git_config() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    repo.config_handle()
        .set_local("user.name", "Handle Tester")
        .expect("set local");
    assert_eq!(git(wt, &["config", "user.name"]).trim(), "Handle Tester");
    assert_eq!(
        repo.config_handle()
            .get("user.name")
            .expect("get")
            .as_deref(),
        Some("Handle Tester")
    );
}

#[test]
fn rev_walk_resolve_and_list_match_git() {
    let history = two_commit_history();
    let repo = open_grit_repo(history.worktree());
    let wt = history.worktree();

    let head = repo.rev_walk().resolve("HEAD").expect("resolve HEAD");
    let git_head = git(wt, &["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(head.to_hex(), git_head);

    let opts = RevListOptions {
        reverse: false,
        ..RevListOptions::default()
    };
    let result = repo.rev_walk().walk("HEAD", &opts).expect("rev-list");
    let grit_hex: Vec<String> = result.commits.iter().map(|c| c.to_hex()).collect();

    let git_list = git(wt, &["rev-list", "HEAD"]);
    let git_hex: Vec<String> = git_list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(grit_hex, git_hex);
}

#[test]
fn diff_tree_to_tree_matches_git_diff_raw() {
    let history = two_commit_history();
    let repo = open_grit_repo(history.worktree());
    let wt = history.worktree();

    let head = repo.rev_walk().resolve("HEAD").expect("head");
    let parent = repo.rev_walk().resolve("HEAD~1").expect("parent");

    let head_tree = grit_lib::diff::tree_oid_of_commit(&repo.odb, &head).expect("head tree");
    let parent_tree = grit_lib::diff::tree_oid_of_commit(&repo.odb, &parent).expect("parent tree");

    let entries = repo
        .diff()
        .tree_to_tree(Some(&parent_tree), Some(&head_tree))
        .expect("tree diff");
    let grit_raw = entries_raw(&entries);

    let git_raw = git(
        wt,
        &[
            "-c",
            "core.abbrev=40",
            "diff",
            "--raw",
            "--full-index",
            &format!("{}..{}", parent.to_hex(), head.to_hex()),
        ],
    );
    assert_eq!(normalize_raw(&grit_raw), normalize_raw(&git_raw));
}

#[test]
fn diff_index_to_tree_matches_git_diff_cached_raw() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    fs::write(wt.join("staged.txt"), "cached\n").expect("write");
    git(wt, &["add", "staged.txt"]);

    let idx = repo.index_handle().load().expect("load");
    let head_tree = repo
        .rev_walk()
        .resolve("HEAD")
        .ok()
        .and_then(|oid| grit_lib::diff::tree_oid_of_commit(&repo.odb, &oid).ok());

    let entries = repo
        .diff()
        .index_to_tree(&idx, head_tree.as_ref(), false)
        .expect("index diff");
    let grit_raw = entries_raw(&entries);

    let git_raw = git(
        wt,
        &[
            "-c",
            "core.abbrev=40",
            "diff",
            "--cached",
            "--raw",
            "--full-index",
        ],
    );
    assert_eq!(normalize_raw(&grit_raw), normalize_raw(&git_raw));
}

#[test]
fn diff_worktree_to_index_matches_git_diff_raw() {
    let (test, repo) = seeded_files_repo();
    let wt = test.worktree();

    fs::write(wt.join("dirty.txt"), "clean\n").expect("seed");
    git(wt, &["add", "dirty.txt"]);
    fs::write(wt.join("dirty.txt"), "dirty\n").expect("modify");

    let mut idx = repo.index_handle().load().expect("load");
    let entries = repo
        .diff()
        .worktree_to_index(&mut idx, true, false)
        .expect("worktree diff");
    let grit_raw = entries_raw(&entries);

    let git_raw = git(
        wt,
        &["-c", "core.abbrev=40", "diff", "--raw", "--full-index"],
    );
    assert_eq!(normalize_raw(&grit_raw), normalize_raw(&git_raw));
}

fn entries_raw(entries: &[DiffEntry]) -> String {
    entries
        .iter()
        .map(format_raw)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_raw(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn two_commit_history() -> HistoryRepo {
    let history = HistoryRepo::init();
    let wt = history.worktree();
    commit_file(wt, "a.txt", "1\n", "c1");
    commit_file(wt, "b.txt", "2\n", "c2");
    history
}

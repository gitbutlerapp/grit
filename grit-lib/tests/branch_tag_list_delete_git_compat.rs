//! Branch and tag listing/deletion through `grit_lib::porcelain::{branch, tag}`,
//! checked against the system `git` on a repository it wrote (packed refs, an
//! upstream, a linked worktree), plus the reftable backend.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::{create_branch, delete_branch, list_branches};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::tag::{create_tag, delete_tag, list_tags, CreateTagRequest, TagKind};
use grit_lib::progress::NullProgress;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_cmd};

fn oid(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

fn ref_exists(dir: &Path, refname: &str) -> bool {
    git_cmd(&["show-ref", "--verify", "-q", refname])
        .in_dir(dir)
        .exec()
        .ok()
}

fn for_each_ref(dir: &Path, prefix: &str) -> Vec<(String, ObjectId)> {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:lstrip=2) %(objectname)",
            prefix,
        ],
    )
    .lines()
    .map(|line| {
        let (name, oid) = line.split_once(' ').expect("name oid");
        (name.to_owned(), oid.parse().expect("oid"))
    })
    .collect()
}

/// `main` with `merged` (behind HEAD), `side` (ahead of HEAD), `tracked` (ahead of
/// HEAD but merged into its upstream), `wt` checked out in a linked worktree, and
/// lightweight, annotated and tree tags — all packed.
fn git_fixture(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "c1");
    git(&repo, &["branch", "merged"]);
    commit(&repo, "c2");
    git(&repo, &["switch", "-q", "-c", "side"]);
    commit(&repo, "c3");
    git(&repo, &["switch", "-q", "main"]);
    git(&repo, &["branch", "tracked", "side"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/tracked", "side"],
    );
    git(&repo, &["remote", "add", "origin", "/nonexistent"]);
    git(&repo, &["config", "branch.tracked.remote", "origin"]);
    git(
        &repo,
        &["config", "branch.tracked.merge", "refs/heads/tracked"],
    );
    git(&repo, &["tag", "v1", "main~1"]);
    git(&repo, &["tag", "-a", "v2", "-m", "release", "main"]);
    let tree = git(&repo, &["rev-parse", "main^{tree}"]);
    git(&repo, &["tag", "tree-tag", tree.trim()]);
    git(&repo, &["pack-refs", "--all"]);
    let wt = root.join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()],
    );
    repo
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

#[test]
fn list_branches_matches_git_for_each_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let listing = list_branches(&open(&dir)).unwrap();

    assert_eq!(listing.current.as_deref(), Some("main"));
    let ours: Vec<(String, ObjectId)> = listing
        .branches
        .iter()
        .map(|b| (b.name.clone(), b.oid))
        .collect();
    assert_eq!(ours, for_each_ref(&dir, "refs/heads/"));
    let current: Vec<&str> = listing
        .branches
        .iter()
        .filter(|b| b.current)
        .map(|b| b.name.as_str())
        .collect();
    assert_eq!(current, ["main"]);
}

#[test]
fn list_tags_matches_git_and_classifies_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let tags = list_tags(&open(&dir)).unwrap();

    let ours: Vec<(String, ObjectId)> = tags.iter().map(|t| (t.name.clone(), t.oid)).collect();
    assert_eq!(ours, for_each_ref(&dir, "refs/tags/"));
    for tag in &tags {
        let git_type = git(&dir, &["cat-file", "-t", &tag.oid.to_hex()]);
        let expected_kind = if git_type.trim() == "tag" {
            TagKind::Annotated
        } else {
            TagKind::Lightweight
        };
        assert_eq!(tag.kind, expected_kind, "{tag:?}");
        let peeled = git_cmd(&[
            "rev-parse",
            "-q",
            "--verify",
            &format!("{}^{{commit}}", tag.name),
        ])
        .in_dir(&dir)
        .exec();
        let expected_peeled = peeled
            .ok()
            .then(|| peeled.stdout.trim().parse::<ObjectId>().expect("oid"));
        assert_eq!(tag.peeled, expected_peeled, "{tag:?}");
    }
}

/// grit's merged check agrees with `git branch -d` on a copy of the same repository.
#[test]
fn delete_branch_merged_check_agrees_with_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let repo = open(&dir);

    for (name, merged) in [("merged", true), ("tracked", true), ("side", false)] {
        let copy = tmp.path().join(format!("copy-{name}"));
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--mirror",
                dir.to_str().unwrap(),
                copy.to_str().unwrap(),
            ],
        );
        // Map `origin` like a normal clone so `tracked`'s upstream is the remote-tracking ref.
        git(&copy, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(
            &copy,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(&copy, &["config", "branch.tracked.remote", "origin"]);
        git(
            &copy,
            &["config", "branch.tracked.merge", "refs/heads/tracked"],
        );
        let git_deleted = git_cmd(&["branch", "-d", name]).in_dir(&copy).exec().ok();
        assert_eq!(git_deleted, merged, "git branch -d {name}");

        let tip = oid(&dir, name);
        let ours = delete_branch(&repo, name, false);
        assert_eq!(ours.is_ok(), git_deleted, "{name}: {ours:?}");
        match ours {
            Ok(deleted) => {
                assert_eq!((deleted.name.as_str(), deleted.oid), (name, tip));
                assert!(!ref_exists(&dir, &format!("refs/heads/{name}")));
                assert!(!dir.join(format!(".git/logs/refs/heads/{name}")).exists());
            }
            Err(err) => {
                assert!(
                    matches!(&err, Error::BranchNotMerged { branch } if branch == name),
                    "{err:?}"
                );
                assert_eq!(oid(&dir, name), tip);
            }
        }
    }

    let forced = delete_branch(&repo, "side", true).unwrap();
    assert_eq!(forced.oid, oid(&dir, "refs/remotes/origin/tracked"));
    assert!(!ref_exists(&dir, "refs/heads/side"));
    git(&dir, &["fsck", "--no-progress"]);
}

#[test]
fn delete_branch_refuses_checked_out_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let repo = open(&dir);

    let err = delete_branch(&repo, "main", true).unwrap_err();
    assert!(
        matches!(&err, Error::CannotDeleteCurrentBranch { branch } if branch == "main"),
        "{err:?}"
    );
    let err = delete_branch(&repo, "wt", true).unwrap_err();
    assert!(
        matches!(&err, Error::BranchCheckedOutInWorktree { branch, .. } if branch == "wt"),
        "{err:?}"
    );
    let err = delete_branch(&repo, "missing", true).unwrap_err();
    assert!(
        matches!(&err, Error::BranchNotFound { branch } if branch == "missing"),
        "{err:?}"
    );
    assert!(ref_exists(&dir, "refs/heads/main"));
    assert!(ref_exists(&dir, "refs/heads/wt"));
}

#[test]
fn delete_packed_tags_is_seen_by_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let repo = open(&dir);
    let annotated = oid(&dir, "refs/tags/v2");

    assert_eq!(delete_tag(&repo, "v2").unwrap(), annotated);
    assert_eq!(delete_tag(&repo, "v1").unwrap(), oid(&dir, "main~1"));
    assert!(!ref_exists(&dir, "refs/tags/v2"));
    assert!(!ref_exists(&dir, "refs/tags/v1"));
    assert_eq!(
        git(&dir, &["cat-file", "-t", &annotated.to_hex()]).trim(),
        "tag"
    );
    assert_eq!(
        for_each_ref(&dir, "refs/tags/")
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["tree-tag"]
    );
    let err = delete_tag(&repo, "v2").unwrap_err();
    assert!(
        matches!(&err, Error::TagNotFound { tag } if tag == "v2"),
        "{err:?}"
    );
    git(&dir, &["fsck", "--no-progress"]);
}

#[test]
fn list_and_delete_on_reftable_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .unwrap();
    let ident = "T <t@example.com> 1700000000 +0000";
    let tip = create_commit(
        &repo,
        &CommitRequest {
            message: "one".into(),
            author: ident.into(),
            committer: ident.into(),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        },
        &mut NullProgress,
    )
    .unwrap()
    .oid;
    create_branch(&repo, "topic").unwrap();
    create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: tip,
            message: None,
            tagger: None,
        },
    )
    .unwrap();

    let names: Vec<String> = list_branches(&repo)
        .unwrap()
        .branches
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["main", "topic"]);
    assert_eq!(list_tags(&repo).unwrap().len(), 1);

    assert_eq!(delete_branch(&repo, "topic", false).unwrap().oid, tip);
    assert_eq!(delete_tag(&repo, "v1").unwrap(), tip);
    let names: Vec<String> = list_branches(&repo)
        .unwrap()
        .branches
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["main"]);
    assert!(list_tags(&repo).unwrap().is_empty());
}

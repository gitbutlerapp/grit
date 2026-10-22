//! Branch creation in a files-backend repository with many packed refs, checked against the
//! system `git` binary: the write touches only the new ref and its reflog, and refs that
//! block the new name (ancestors and descendants, loose or packed) are still reported.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions, TrackMode};
use grit_lib::refs::store::RefStoreError;
use grit_lib::refs::RefnameUnavailable;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn create(dir: &Path, name: &str) -> grit_lib::error::Result<()> {
    create_branch(
        &open(dir),
        &CreateBranchOptions {
            name: name.into(),
            start: None,
            force: false,
            track: TrackMode::Never,
            dry_run: false,
        },
    )
    .map(|_| ())
}

/// A repository whose `main` has one commit, with `count` packed branches and tags.
fn repo_with_packed_refs(count: usize) -> (tempfile::TempDir, ObjectId) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "core.logAllRefUpdates", "true"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    let head: ObjectId = git(dir, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let mut stdin = String::new();
    for i in 0..count {
        writeln!(stdin, "create refs/heads/b{i:05} {head}").unwrap();
        writeln!(stdin, "create refs/tags/t{i:05} {head}").unwrap();
    }
    git_cmd(&["update-ref", "--stdin"])
        .in_dir(dir)
        .stdin(stdin)
        .suc();
    git(dir, &["pack-refs", "--all"]);
    (tmp, head)
}

fn lock_files(dir: &Path) -> Vec<String> {
    fn walk(path: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "lock") {
                out.push(p.display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(&dir.join(".git"), &mut out);
    out
}

fn name_unavailable(err: Error) -> RefnameUnavailable {
    match err {
        Error::RefStore(RefStoreError::NameUnavailable { reason }) => reason,
        other => panic!("expected a refname conflict, got {other:?}"),
    }
}

#[test]
fn create_among_many_packed_refs_writes_only_the_new_ref_and_log() {
    let (tmp, head) = repo_with_packed_refs(2000);
    let dir = tmp.path();
    git(dir, &["branch", "loose-sibling"]);
    let packed_before = fs::read(dir.join(".git/packed-refs")).unwrap();
    let refs_before = git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]);

    create(dir, "topic").unwrap();

    assert_eq!(
        fs::read(dir.join(".git/packed-refs")).unwrap(),
        packed_before,
        "creating a loose ref must not rewrite packed-refs"
    );
    let refs_after = git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let mut expected: Vec<&str> = refs_before.lines().collect();
    let new_line = format!("refs/heads/topic {head}");
    expected.push(&new_line);
    expected.sort_unstable();
    assert_eq!(refs_after.lines().collect::<Vec<_>>(), expected);
    assert_eq!(
        fs::read_to_string(dir.join(".git/refs/heads/topic")).unwrap(),
        format!("{head}\n")
    );
    assert_eq!(
        git(
            dir,
            &["reflog", "show", "--format=%H %gs", "refs/heads/topic"]
        ),
        format!("{head} branch: Created from main\n")
    );
    git(dir, &["fsck", "--no-progress"]);
    assert!(lock_files(dir).is_empty(), "{:?}", lock_files(dir));
}

#[test]
fn packed_and_loose_ancestors_block_create_like_git() {
    let (tmp, _) = repo_with_packed_refs(50);
    let dir = tmp.path();
    git(dir, &["branch", "loose-parent"]);
    for (parent, child) in [
        ("b00007", "b00007/child"),
        ("loose-parent", "loose-parent/child"),
    ] {
        assert!(
            !git_cmd(&["branch", child]).in_dir(dir).exec().ok(),
            "git refuses {child}"
        );
        let reason = name_unavailable(create(dir, child).unwrap_err());
        assert_eq!(
            reason,
            RefnameUnavailable::AncestorExists {
                blocking: format!("refs/heads/{parent}"),
                new_ref: format!("refs/heads/{child}"),
            }
        );
        assert!(!dir.join(".git/refs/heads").join(child).exists());
    }
    git(dir, &["fsck", "--no-progress"]);
    assert!(lock_files(dir).is_empty(), "{:?}", lock_files(dir));
}

#[test]
fn packed_descendants_block_create_like_git() {
    let (tmp, head) = repo_with_packed_refs(50);
    let dir = tmp.path();
    git(dir, &["update-ref", "refs/heads/nest/b", &head.to_hex()]);
    git(dir, &["update-ref", "refs/heads/nest/a", &head.to_hex()]);
    git(dir, &["pack-refs", "--all"]);
    // Git leaves the reflogs of the packed refs in `logs/refs/heads/nest/`.
    assert!(dir.join(".git/logs/refs/heads/nest").is_dir());
    assert!(!git_cmd(&["branch", "nest"]).in_dir(dir).exec().ok());

    let reason = name_unavailable(create(dir, "nest").unwrap_err());
    assert_eq!(
        reason,
        RefnameUnavailable::DescendantExists {
            blocking: "refs/heads/nest/a".into(),
            new_ref: "refs/heads/nest".into(),
        }
    );
    assert!(!dir.join(".git/refs/heads/nest").is_file());
    git(dir, &["fsck", "--no-progress"]);
    assert!(lock_files(dir).is_empty(), "{:?}", lock_files(dir));
}

#[test]
fn branch_created_next_to_packed_prefix_siblings() {
    let (tmp, head) = repo_with_packed_refs(50);
    let dir = tmp.path();
    git(dir, &["update-ref", "refs/heads/nest-x/a", &head.to_hex()]);
    git(dir, &["update-ref", "refs/heads/nestz", &head.to_hex()]);
    git(dir, &["pack-refs", "--all"]);

    create(dir, "nest").unwrap();
    create(dir, "b00001-x").unwrap();
    assert_eq!(
        git(dir, &["rev-parse", "nest", "b00001-x"]),
        format!("{head}\n{head}\n")
    );
    git(dir, &["fsck", "--no-progress"]);
}

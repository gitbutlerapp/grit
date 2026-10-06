//! Regression tests for bounded `rev-list` / `grit log` fast paths.

use grit_lib::error::Error;
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use grit_lib::repo::{init_bare_clone_minimal, Repository};
use grit_lib::rev_list::{rev_list, OrderingMode, RevListOptions};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(odb: &Odb) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    odb.write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(
    odb: &Odb,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
    author_time: i64,
    committer_time: i64,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let tree = empty_tree(odb)?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {author_time} +0000\ncommitter T <t@e.com> {committer_time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    init_bare_clone_minimal(dir, "main", "files")?;
    Repository::open(dir, None)
}

#[test]
fn bounded_log_on_depth_one_shallow_does_not_emit_missing_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(&repo.odb, &[missing], "tip", 100, 100).expect("commit");

    fs::write(repo.git_dir.join("shallow"), format!("{tip}\n")).expect("shallow");

    let opts = RevListOptions {
        max_count: Some(11),
        ..Default::default()
    };
    let result = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![tip]);
}

#[test]
fn rev_list_errors_on_missing_parent_without_shallow() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(&repo.odb, &[missing], "tip", 100, 100).expect("commit");

    let opts = RevListOptions {
        max_count: Some(5),
        ..Default::default()
    };
    let err = rev_list(&repo, &[tip.to_hex()], &[], &opts).unwrap_err();
    assert!(matches!(err, Error::ObjectNotFound(_)));
}

#[test]
fn author_date_order_with_max_count_uses_author_timestamps() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(&repo.odb, &[], "root", 50, 50).expect("root");
    // `a`: older author, newer committer.
    let a = write_commit(&repo.odb, &[root], "a", 100, 400).expect("a");
    // `b`: newer author, older committer.
    let b = write_commit(&repo.odb, &[root], "b", 300, 200).expect("b");

    let opts = RevListOptions {
        max_count: Some(2),
        ordering: OrderingMode::AuthorDateWalk,
        ..Default::default()
    };
    let result = rev_list(&repo, &[a.to_hex(), b.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![b, a]);
}

#[test]
fn exclude_first_parent_only_with_max_count_matches_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(&repo.odb, &[], "root", 10, 10).expect("root");
    let first = write_commit(&repo.odb, &[root], "first", 20, 20).expect("first");
    let second = write_commit(&repo.odb, &[root], "second", 30, 30).expect("second");
    let merge = write_commit(&repo.odb, &[first, second], "merge", 40, 40).expect("merge");

    let opts = RevListOptions {
        max_count: Some(1),
        exclude_first_parent_only: true,
        ..Default::default()
    };
    let result = rev_list(&repo, &[second.to_hex()], &[merge.to_hex()], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![second]);
}

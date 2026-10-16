//! Tests for [`grit_lib::history::reword`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::history::{
    normalize_rewrite_message, reword, EmptyPolicy, HistoryError, RefScope, RewriteOptions,
};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;

use support::{build_commit_dag, git, git_fsck_strict, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn committer(seq: i64) -> String {
    format!("Rewriter <r@grit.test> {seq} +0000")
}

#[test]
fn reword_changes_only_message_and_replays_descendants() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let plan = reword(
        &grit,
        oids["b"],
        "second reworded\n".into(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(1),
        },
    )
    .expect("reword");
    let b_row = plan
        .commits
        .iter()
        .find(|r| r.old == oids["b"])
        .expect("b row");
    let new_b = b_row.new.expect("new b");
    let old_b = grit.odb().read(&oids["b"]).expect("read old b");
    let new_b_obj = grit.odb().read(&new_b).expect("read new b");
    let old_parsed = parse_commit(&old_b.data).expect("parse old");
    let new_parsed = parse_commit(&new_b_obj.data).expect("parse new");
    assert_eq!(old_parsed.tree, new_parsed.tree);
    assert_eq!(old_parsed.parents, new_parsed.parents);
    assert_eq!(old_parsed.author, new_parsed.author);
    assert_eq!(new_parsed.message, "second reworded\n");
    assert_ne!(oids["b"], new_b);
    let c_row = plan
        .commits
        .iter()
        .find(|r| r.old == oids["c"])
        .expect("c row");
    assert!(c_row.new.is_some());
    assert_ne!(c_row.new, Some(oids["c"]));
    git_fsck_strict(wt);
}

#[test]
fn reword_rejects_empty_message() {
    let err = normalize_rewrite_message("   \n\n  ").unwrap_err();
    assert_eq!(err, HistoryError::EmptyMessage);
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = reword(
        &grit,
        oids["b"],
        "  \t\n".into(),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(1),
        },
    )
    .unwrap_err();
    assert_eq!(err, HistoryError::EmptyMessage);
}

#[test]
fn reword_in_bare_repository() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    git(
        tmp.path(),
        &["init", "--bare", "-b", "main", tmp.path().to_str().unwrap()],
    );
    let grit = Repository::discover(
        Some(tmp.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open bare");
    let empty = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Tree, &[])
        .expect("tree");
    let commit_bytes = grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData {
        tree: empty,
        parents: vec![],
        author: committer(0),
        committer: committer(0),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "root\n".into(),
        raw_message: None,
        extra_headers: vec![],
    });
    let oid = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Commit, &commit_bytes)
        .expect("commit");
    grit_lib::refs::write_ref(grit.git_dir(), "refs/heads/main", &oid).expect("ref");
    let plan = reword(
        &grit,
        oid,
        "bare reword\n".into(),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(2),
        },
    )
    .expect("reword bare");
    let new = plan
        .commits
        .iter()
        .find(|r| r.old == oid)
        .and_then(|r| r.new)
        .expect("replacement");
    assert_ne!(new, oid);
    let parsed = parse_commit(&grit.odb().read(&new).expect("read").data).expect("parse");
    assert_eq!(parsed.message, "bare reword\n");
    let _unused: ObjectId = new;
}

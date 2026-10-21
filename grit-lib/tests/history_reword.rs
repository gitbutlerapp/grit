//! Tests for [`grit_lib::revwalk::history::reword`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::objects::{parse_commit, parse_tree, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, normalize_rewrite_message, reword, ApplyOptions, EmptyPolicy, HistoryError,
    RefScope, RewriteOptions,
};
use grit_lib::state::resolve_head;

use support::{build_commit_dag, git, git_fsck_strict, git_rev_parse, HistoryRepo};

fn tree_contains_path(repo: &Repository, tree: ObjectId, path: &str) -> bool {
    let raw = repo.odb().read(&tree).expect("read tree");
    parse_tree(&raw.data)
        .expect("parse tree")
        .iter()
        .any(|e| e.name.as_slice() == path.as_bytes())
}

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
            preserve_committer: false,
            sign: true,
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
    assert!(matches!(err, HistoryError::EmptyMessage));
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
            preserve_committer: false,
            sign: true,
        },
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::EmptyMessage));
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
            preserve_committer: false,
            sign: true,
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

#[test]
fn reword_root_replays_merge_with_merge_only_tree_content() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "user.email", "rewriter@test"]);
    git(wt, &["config", "user.name", "Rewriter"]);
    std::fs::write(wt.join("root.txt"), "root\n").expect("root file");
    git(wt, &["add", "root.txt"]);
    git(wt, &["commit", "-m", "root"]);
    let root = git_rev_parse(wt, "HEAD").expect("root");

    std::fs::write(wt.join("main.txt"), "main\n").expect("main file");
    git(wt, &["add", "main.txt"]);
    git(wt, &["commit", "-m", "mainline"]);

    git(wt, &["branch", "side"]);
    git(wt, &["checkout", "side"]);
    std::fs::write(wt.join("side.txt"), "side\n").expect("side file");
    git(wt, &["add", "side.txt"]);
    git(wt, &["commit", "-m", "side branch"]);

    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "--no-ff", "-m", "merge side"]);
    std::fs::write(wt.join("merge-only.txt"), "merge-only\n").expect("merge-only");
    git(wt, &["add", "merge-only.txt"]);
    git(wt, &["commit", "--amend", "--no-edit"]);

    let merge_before = parse_commit(
        &open_grit(wt)
            .odb()
            .read(&git_rev_parse(wt, "HEAD").expect("head"))
            .expect("read")
            .data,
    )
    .expect("parse merge");
    assert!(
        tree_contains_path(&open_grit(wt), merge_before.tree, "merge-only.txt"),
        "fixture merge must contain merge-only.txt"
    );

    let grit = open_grit(wt);
    let plan = reword(
        &grit,
        root,
        "rewritten root\n".into(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(10),
            preserve_committer: false,
            sign: true,
        },
    )
    .expect("reword root");
    apply_rewrite(
        &grit,
        &plan,
        &ApplyOptions {
            identity: "Rewriter <rewriter@test>".into(),
            reflog_message: "history reword".into(),
        },
    )
    .expect("apply");

    let head = resolve_head(grit.git_dir()).expect("head");
    let head_oid = head.oid().copied().expect("head oid");
    let head_tree = parse_commit(&grit.odb().read(&head_oid).expect("read").data)
        .expect("parse head")
        .tree;
    assert!(
        tree_contains_path(&grit, head_tree, "merge-only.txt"),
        "replayed merge must retain merge-only.txt"
    );
    assert!(
        tree_contains_path(&grit, head_tree, "main.txt"),
        "mainline content must remain"
    );
    assert!(
        tree_contains_path(&grit, head_tree, "side.txt"),
        "side branch content must remain"
    );
    git_fsck_strict(wt);
}

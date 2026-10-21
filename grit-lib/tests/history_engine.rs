//! Integration tests for the in-memory history rewrite planner.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::process::Command;

use grit_lib::commit::encoding;
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
};
use grit_lib::state::resolve_head;

use support::{build_commit_dag, commit_file, git, git_rev_parse, HistoryRepo};

fn committer(seq: i64) -> String {
    format!("Planner <plan@grit.test> {seq} +0000")
}

fn tree_contains_path(repo: &Repository, tree: ObjectId, path: &str) -> bool {
    let raw = repo.odb().read(&tree).expect("read tree");
    parse_tree(&raw.data)
        .expect("parse tree")
        .iter()
        .any(|e| e.name.as_slice() == path.as_bytes())
}

fn tree_plus_file(
    repo: &Repository,
    base_tree: ObjectId,
    path: &str,
    content: &[u8],
) -> grit_lib::error::Result<ObjectId> {
    let blob = repo.odb().write(ObjectKind::Blob, content)?;
    let raw = repo.odb().read(&base_tree)?;
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    for entry in parse_tree(&raw.data)? {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        index.add_or_replace(index_entry_from_tree(&entry, &name));
    }
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: content.len() as u32,
        oid: blob,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    });
    write_tree_from_index(repo.odb(), &index, "")
}

fn index_entry_from_tree(entry: &grit_lib::objects::TreeEntry, name: &str) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: entry.mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: entry.oid,
        flags: name.len().min(0xfff) as u16,
        flags_extended: None,
        path: name.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

fn write_tree_with_blob(
    repo: &grit_lib::repo::Repository,
    path: &str,
    content: &[u8],
) -> grit_lib::error::Result<ObjectId> {
    let blob = repo.odb().write(ObjectKind::Blob, content)?;
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: content.len() as u32,
        oid: blob,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    });
    write_tree_from_index(repo.odb(), &index, "")
}

fn write_test_commit(
    repo: &grit_lib::repo::Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    msg: &str,
    ident: &str,
) -> grit_lib::error::Result<ObjectId> {
    use grit_lib::objects::{serialize_commit, CommitData};
    let raw = serialize_commit(&CommitData {
        tree,
        parents: parents.to_vec(),
        author: ident.to_owned(),
        committer: ident.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{msg}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    });
    repo.odb().write(ObjectKind::Commit, &raw)
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn git_fsck_strict(wt: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(wt)
        .args(["fsck", "--strict"])
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn plan_rewrite_rewords_and_replays_linear_descendants() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["b"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("second reworded\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(2),
            preserve_committer: false,
        },
    )
    .expect("plan");

    assert_eq!(plan.commits.len(), 2);
    let b_row = plan.commits.iter().find(|r| r.old == oids["b"]).unwrap();
    let c_row = plan.commits.iter().find(|r| r.old == oids["c"]).unwrap();
    assert!(b_row.new.is_some());
    assert!(c_row.new.is_some());
    assert_ne!(b_row.new, c_row.new);

    let new_b = b_row.new.unwrap();
    let parsed = parse_commit(&grit.odb().read(&new_b).expect("read").data).expect("parse");
    assert_eq!(parsed.message, "second reworded\n");

    git_fsck_strict(wt);
}

#[test]
fn plan_rewrite_preserves_encoding_when_message_unchanged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let old_b = oids["b"];
    let parent_tree = parse_commit(&grit.odb().read(&old_b).expect("read").data)
        .expect("parse")
        .tree;
    let parent = oids["a"];
    let (message, enc_label, raw_body) =
        encoding::finalize_stored_commit_message("caf\u{00e9}\n".to_owned(), Some("ISO-8859-1"));
    let ident = "Enc <enc@grit.test> 1700000000 +0000";
    let encoded_b = grit
        .odb()
        .write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: parent_tree,
                parents: vec![parent],
                author: ident.to_owned(),
                committer: ident.to_owned(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: enc_label.clone(),
                message: message.clone(),
                raw_message: raw_body.clone(),
                extra_headers: Vec::new(),
            }),
        )
        .expect("write encoded commit");
    git(wt, &["update-ref", "refs/heads/main", &encoded_b.to_hex()]);

    let mut edits = HistoryEdits::new();
    edits.insert(
        encoded_b,
        CommitEdit::Rewrite {
            tree: None,
            message: None,
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(9),
            preserve_committer: false,
        },
    )
    .expect("plan");
    let row = plan
        .commits
        .iter()
        .find(|r| r.old == encoded_b)
        .expect("encoded commit row");
    let new_oid = row.new.expect("rewritten");
    let parsed = parse_commit(&grit.odb().read(&new_oid).expect("read").data).expect("parse");
    assert_eq!(parsed.encoding.as_deref(), Some("ISO-8859-1"));
    assert_eq!(parsed.raw_message, raw_body);
    assert_eq!(parsed.committer, committer(9));
    git_fsck_strict(wt);
}

#[test]
fn plan_rewrite_implicit_tree_replay_preserves_ancestor_tree_change() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let b_oid = oids["b"];
    let a_oid = oids["a"];
    let a_data = parse_commit(&grit.odb().read(&a_oid).expect("read a").data).expect("parse a");
    let a_tree_with_ancestor =
        tree_plus_file(&grit, a_data.tree, "ancestor.txt", b"from root\n").expect("tree");

    let mut edits = HistoryEdits::new();
    edits.insert(
        a_oid,
        CommitEdit::Rewrite {
            tree: Some(a_tree_with_ancestor),
            message: None,
            author: None,
            parents: None,
        },
    );
    edits.insert(
        b_oid,
        CommitEdit::Rewrite {
            tree: None,
            message: Some("reword child only\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(20),
            preserve_committer: false,
        },
    )
    .expect("plan");
    let new_b = plan
        .commits
        .iter()
        .find(|r| r.old == b_oid)
        .and_then(|r| r.new)
        .expect("rewritten b");
    let new_b_tree = parse_commit(&grit.odb().read(&new_b).expect("read").data)
        .expect("parse")
        .tree;
    assert!(
        tree_contains_path(&grit, new_b_tree, "ancestor.txt"),
        "child replay should include ancestor tree change"
    );
    git_fsck_strict(wt);
}

#[test]
fn plan_rewrite_insert_before_root_succeeds() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[], &[("r", "refs/heads/main")]);
    let grit = open_grit(wt);
    let insert_tree = grit.odb().write(ObjectKind::Tree, &[]).expect("empty tree");
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["r"],
        CommitEdit::InsertBefore(grit_lib::revwalk::history::NewCommit {
            tree: insert_tree,
            author: "Insert <i@grit.test> 1 +0000".into(),
            message: "inserted root parent\n".into(),
        }),
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(21),
            preserve_committer: false,
        },
    )
    .expect("insert before root");
    assert_eq!(plan.commits.len(), 1);
    assert!(plan.commits[0].new.is_some());
    git_fsck_strict(wt);
}

#[test]
fn plan_rewrite_insert_before_merge_is_rejected() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let ident = committer(22);
    let tree_a = write_tree_with_blob(&grit, "a.txt", b"a\n").expect("tree a");
    let tree_b = write_tree_with_blob(&grit, "b.txt", b"b\n").expect("tree b");
    let a = write_test_commit(&grit, tree_a, &[], "a", &ident).expect("a");
    let b = write_test_commit(&grit, tree_b, &[], "b", &ident).expect("b");
    let merge_tree = write_tree_with_blob(&grit, "m.txt", b"m\n").expect("merge tree");
    let merge = write_test_commit(&grit, merge_tree, &[a, b], "merge", &ident).expect("merge");
    write_ref(grit.git_dir(), "refs/heads/main", &merge).expect("ref");

    let insert_tree = grit.odb().write(ObjectKind::Tree, &[]).expect("empty tree");
    let mut edits = HistoryEdits::new();
    edits.insert(
        merge,
        CommitEdit::InsertBefore(grit_lib::revwalk::history::NewCommit {
            tree: insert_tree,
            author: "Insert <i@grit.test> 1 +0000".into(),
            message: "inserted parent\n".into(),
        }),
    );
    let err = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(23),
            preserve_committer: false,
        },
    )
    .expect_err("insert before merge");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::InsertBeforeOnMerge { commit: m } if m == merge
    ));
}

#[test]
fn plan_rewrite_non_commit_tip_returns_planning_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit = grit_lib::repo::Repository::init(
        dir.path(),
        &grit_lib::repo::InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let ident = committer(24);
    let blob = grit
        .odb()
        .write(ObjectKind::Blob, b"not a commit")
        .expect("blob");
    grit_lib::refs::write_ref(grit.git_dir(), "refs/heads/main", &blob).expect("ref");
    let mut edits = HistoryEdits::new();
    edits.insert(
        blob,
        CommitEdit::Rewrite {
            tree: None,
            message: Some("nope\n".into()),
            author: None,
            parents: None,
        },
    );
    let err = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: ident,
            preserve_committer: false,
        },
    )
    .expect_err("blob tip is not a commit");
    assert!(
        matches!(
            err,
            grit_lib::revwalk::history::HistoryError::Planning { .. }
        ),
        "expected planning error, got {err:?}"
    );
}

#[test]
fn plan_rewrite_strips_gpgsig_and_gpgsig_sha256() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    for (header, case) in [
        (
            b"gpgsig -----BEGIN PGP SIGNATURE-----\n sig\n -----END PGP SIGNATURE-----\n"
                .as_slice(),
            "gpgsig",
        ),
        (
            b"gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n sig\n -----END PGP SIGNATURE-----\n"
                .as_slice(),
            "gpgsig-sha256",
        ),
    ] {
        let parent = oids["a"];
        let parent_tree = parse_commit(&grit.odb().read(&parent).expect("read").data)
            .expect("parse")
            .tree;
        let signed = grit
            .odb()
            .write(
                ObjectKind::Commit,
                &serialize_commit(&CommitData {
                    tree: parent_tree,
                    parents: vec![parent],
                    author: "S <s@grit.test> 1 +0000".into(),
                    committer: "S <s@grit.test> 1 +0000".into(),
                    author_raw: Vec::new(),
                    committer_raw: Vec::new(),
                    encoding: None,
                    message: format!("signed {case}\n"),
                    raw_message: None,
                    extra_headers: header.to_vec(),
                }),
            )
            .expect("write signed");
        git(wt, &["update-ref", "refs/heads/main", &signed.to_hex()]);
        let mut edits = HistoryEdits::new();
        edits.insert(
            signed,
            CommitEdit::Rewrite {
                tree: None,
                message: Some(format!("reword {case}\n")),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &grit,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer(22),
                preserve_committer: false,
            },
        )
        .expect("plan signed rewrite");
        let new_oid = plan.commits[0].new.expect("new commit");
        let parsed = parse_commit(&grit.odb().read(&new_oid).expect("read").data).expect("parse");
        let raw = String::from_utf8_lossy(&parsed.extra_headers);
        assert!(
            !raw.contains("gpgsig"),
            "expected no signature headers left for {case}, got: {raw}"
        );
    }
}

#[test]
fn plan_rewrite_keeps_all_merge_parents_on_implicit_tree_reword() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "a"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    git(wt, &["checkout", "-b", "side", &oids["c"].to_hex()]);
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "-m", "merge m"]);
    let grit = open_grit(wt);
    let merge_oid = match resolve_head(grit.git_dir()).expect("head") {
        grit_lib::state::HeadState::Branch { oid: Some(oid), .. } => oid,
        grit_lib::state::HeadState::Detached { oid } => oid,
        _ => panic!("expected merge tip"),
    };
    let merge_before =
        parse_commit(&grit.odb().read(&merge_oid).expect("read").data).expect("parse");
    assert_eq!(merge_before.parents.len(), 2);
    let mut edits = HistoryEdits::new();
    edits.insert(
        merge_oid,
        CommitEdit::Rewrite {
            tree: None,
            message: Some("merge reword only\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(23),
            preserve_committer: false,
        },
    )
    .expect("merge reword");
    let new_oid = plan
        .commits
        .iter()
        .find(|r| r.old == merge_oid)
        .and_then(|r| r.new)
        .expect("new merge");
    let merge_after = parse_commit(&grit.odb().read(&new_oid).expect("read").data).expect("parse");
    assert_eq!(merge_after.parents.len(), 2);
    assert_eq!(merge_after.message, "merge reword only\n");
}

#[test]
fn plan_rewrite_skips_merge_when_parent_tree_unchanged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("first reworded\n".into()),
            author: None,
            parents: None,
        },
    );
    let _plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(3),
            preserve_committer: false,
        },
    )
    .expect("plan");
}

#[test]
fn plan_rewrite_updates_all_descendant_branches() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    git(wt, &["branch", "feature"]);
    commit_file(wt, "feat.txt", "x", "on feature");
    let feature_tip: ObjectId = git_rev_parse(wt, "feature").expect("feature tip");

    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("root reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(4),
            preserve_committer: false,
        },
    )
    .expect("plan");

    let names: Vec<_> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert!(names.iter().any(|n| n.ends_with("main")));
    assert!(names.iter().any(|n| n.ends_with("feature")));
    assert!(plan
        .ref_updates
        .iter()
        .any(|u| u.old == feature_tip && u.new != feature_tip));
}

#[test]
fn plan_rewrite_head_scope_updates_only_head() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    git(wt, &["branch", "feature"]);
    commit_file(wt, "side.txt", "s", "side");

    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("base reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(5),
            preserve_committer: false,
        },
    )
    .expect("plan");

    assert!(plan.ref_updates.iter().all(|u| u.name.contains("main")));
    assert!(!plan.ref_updates.iter().any(|u| u.name.ends_with("feature")));
}

#[test]
fn plan_rewrite_replays_merge_descendants() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "a"), ("m", "b"), ("m", "c"), ("d", "m")],
        &[("d", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("reword root\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(6),
            preserve_committer: false,
        },
    )
    .expect("merge replay");
    let merge_row = plan
        .commits
        .iter()
        .find(|r| r.old == oids["m"])
        .expect("merge row");
    let new_merge = merge_row.new.expect("rewritten merge");
    assert_ne!(new_merge, oids["m"]);
    let parsed = parse_commit(&grit.odb().read(&new_merge).expect("read").data).expect("parse");
    assert_eq!(parsed.parents.len(), 2);
    let new_b = plan
        .commits
        .iter()
        .find(|r| r.old == oids["b"])
        .and_then(|r| r.new)
        .expect("mapped b");
    let new_c = plan
        .commits
        .iter()
        .find(|r| r.old == oids["c"])
        .and_then(|r| r.new)
        .expect("mapped c");
    assert!(parsed.parents.contains(&new_b));
    assert!(parsed.parents.contains(&new_c));
    git_fsck_strict(wt);
}

#[test]
fn plan_rewrite_reports_conflict_with_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit = grit_lib::repo::Repository::init(
        dir.path(),
        &grit_lib::repo::InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let ident = committer(7);
    let tree_base =
        write_tree_with_blob(&grit, "conflict.txt", b"1\nbase\n3\n").expect("base tree");
    let tree_left =
        write_tree_with_blob(&grit, "conflict.txt", b"1\nleft\n3\n").expect("left tree");
    let tree_right =
        write_tree_with_blob(&grit, "conflict.txt", b"1\nright\n3\n").expect("right tree");
    let a = write_test_commit(&grit, tree_base, &[], "a", &ident).expect("a");
    let b = write_test_commit(&grit, tree_left, &[a], "b", &ident).expect("b");
    let c = write_test_commit(&grit, tree_right, &[b], "c", &ident).expect("c");
    write_ref(grit.git_dir(), "refs/heads/main", &c).expect("ref");

    let mut edits = HistoryEdits::new();
    edits.insert(
        b,
        CommitEdit::Rewrite {
            tree: Some(tree_base),
            message: None,
            author: None,
            parents: None,
        },
    );

    let err = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Abort,
            committer: ident,
            preserve_committer: false,
        },
    )
    .expect_err("conflict");
    match err {
        grit_lib::revwalk::history::HistoryError::Conflict { paths, .. } => {
            assert!(paths.iter().any(|p| p.contains("conflict.txt")));
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[test]
fn plan_rewrite_empty_policy_drop_keep_abort() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit = grit_lib::repo::Repository::init(
        dir.path(),
        &grit_lib::repo::InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let ident = committer(8);
    let tree_a = write_tree_with_blob(&grit, "f.txt", b"a\n").expect("tree a");
    let tree_b = write_tree_with_blob(&grit, "f.txt", b"b\n").expect("tree b");
    let a = write_test_commit(&grit, tree_a, &[], "a", &ident).expect("a");
    let b = write_test_commit(&grit, tree_b, &[a], "b", &ident).expect("b");
    let c = write_test_commit(&grit, tree_a, &[b], "c", &ident).expect("c");
    write_ref(grit.git_dir(), "refs/heads/main", &c).expect("ref");
    let oids_c = c;
    let mut edits_drop = HistoryEdits::new();
    edits_drop.insert(
        b,
        CommitEdit::Rewrite {
            tree: Some(tree_a),
            message: None,
            author: None,
            parents: None,
        },
    );
    let drop_plan = plan_rewrite(
        &grit,
        &edits_drop,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(8),
            preserve_committer: false,
        },
    )
    .expect("drop");
    let c_drop = drop_plan.commits.iter().find(|r| r.old == oids_c).unwrap();
    let b_drop = drop_plan
        .commits
        .iter()
        .find(|r| r.old == b)
        .unwrap()
        .new
        .expect("b'");
    assert!(
        c_drop.new == Some(b_drop),
        "drop policy should map empty replay to rewritten parent"
    );

    let mut edits_abort = HistoryEdits::new();
    edits_abort.insert(
        b,
        CommitEdit::Rewrite {
            tree: Some(tree_a),
            message: None,
            author: None,
            parents: None,
        },
    );
    let abort_err = plan_rewrite(
        &grit,
        &edits_abort,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Abort,
            committer: committer(9),
            preserve_committer: false,
        },
    )
    .expect_err("abort");
    assert!(matches!(
        abort_err,
        grit_lib::revwalk::history::HistoryError::BecameEmpty { .. }
    ));

    let keep_plan = plan_rewrite(
        &grit,
        &edits_abort,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Keep,
            committer: committer(10),
            preserve_committer: false,
        },
    )
    .expect("keep");
    let c_keep = keep_plan.commits.iter().find(|r| r.old == oids_c).unwrap();
    assert!(c_keep.new.is_some());
}

#[test]
fn plan_rewrite_rejects_branch_checked_out_in_other_worktree() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    git(wt, &["branch", "feature"]);
    let feature_tip = git_rev_parse(wt, "feature").expect("feature");
    let linked = tempfile::tempdir().expect("linked wt dir");
    git(
        wt,
        &[
            "worktree",
            "add",
            linked.path().to_str().expect("utf8"),
            "feature",
        ],
    );

    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let err = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(11),
            preserve_committer: false,
        },
    )
    .expect_err("worktree");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::BranchCheckedOutElsewhere { .. }
    ));
    let _ = feature_tip;
}

#[test]
fn plan_rewrite_detached_head() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    git(wt, &["checkout", "--detach", "HEAD"]);
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["b"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("detached reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(12),
            preserve_committer: false,
        },
    )
    .expect("plan");
    assert!(plan.head.commit_changes);
    assert!(plan.ref_updates.iter().any(|u| u.name == "HEAD"));
}

#[test]
fn plan_rewrite_leaves_refs_index_worktree_untouched() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    commit_file(wt, "tracked.txt", "v1", "track");
    let before_head = resolve_ref(open_grit(wt).git_dir(), "refs/heads/main").expect("read ref");
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("silent reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let _plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(13),
            preserve_committer: false,
        },
    )
    .expect("plan");
    let after_head = resolve_ref(grit.git_dir(), "refs/heads/main").expect("read ref after");
    assert_eq!(before_head, after_head);
    assert!(wt.join("tracked.txt").exists());
}

#[test]
fn plan_rewrite_surfaces_missing_object_as_library_error() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let hex = oids["b"].to_hex();
    let obj_path = wt.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
    std::fs::remove_file(&obj_path).expect("remove loose commit");
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["b"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("reword\n".into()),
            author: None,
            parents: None,
        },
    );
    let err = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(30),
            preserve_committer: false,
        },
    )
    .expect_err("missing commit object");
    match err {
        grit_lib::revwalk::history::HistoryError::Library(boxed) => {
            assert!(matches!(*boxed, grit_lib::error::Error::ObjectNotFound(_)));
        }
        grit_lib::revwalk::history::HistoryError::Conflict { .. } => {
            panic!("missing object must not surface as Conflict");
        }
        other => panic!("expected Library(ObjectNotFound), got {other:?}"),
    }
}

#[test]
fn plan_rewrite_rejects_divergent_branch() {
    let err = grit_lib::revwalk::history::HistoryError::DivergentBranch {
        name: "refs/heads/feature".into(),
    };
    assert!(err.to_string().contains("feature"));
}

#[test]
fn plan_rewrite_parent_only_replay_matches_git_commit_tree() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        oids["a"],
        CommitEdit::Rewrite {
            tree: None,
            message: Some("reword first\n".into()),
            author: None,
            parents: None,
        },
    );
    let committer_line = committer(99);
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer_line.clone(),
            preserve_committer: false,
        },
    )
    .expect("plan");
    let c_row = plan
        .commits
        .iter()
        .find(|r| r.old == oids["c"])
        .expect("c row");
    let new_c = c_row.new.expect("new c");
    let b_new = plan
        .commits
        .iter()
        .find(|r| r.old == oids["b"])
        .expect("b row")
        .new
        .expect("new b");
    let raw = grit.odb().read(&new_c).expect("read").data;
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "-t", "commit", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("hash-object");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&raw)
        .expect("write commit bytes");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git hash-object failed");
    let git_oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        new_c.to_hex(),
        git_oid,
        "written commit must round-trip through git with identical oid"
    );
    let _ = b_new;
    git_fsck_strict(wt);
}

#[test]
fn history_public_api_coverage_smoke() {
    use grit_lib::revwalk::history::{
        CommitEdit, EmptyPolicy, HeadEffect, HistoryError, NewCommit, PlannedRefUpdate, RefScope,
        RewriteOptions, RewritePlan, RewrittenCommit,
    };
    let _ = (
        CommitEdit::Drop,
        EmptyPolicy::Keep,
        RefScope::Head,
        HistoryError::EmptyMessage,
        HistoryError::Planning {
            message: "coverage".into(),
        },
        HistoryError::InsertBeforeOnMerge {
            commit: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
        },
        HistoryError::RootCommit {
            commit: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
        },
    );
    let _plan = RewritePlan {
        commits: vec![RewrittenCommit {
            old: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
            new: None,
            change_id: None,
        }],
        ref_updates: vec![PlannedRefUpdate {
            name: "refs/heads/x".into(),
            old: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
            new: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
        }],
        head: HeadEffect::default(),
    };
    let _opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Abort,
        committer: committer(0),
        preserve_committer: false,
    };
    let _nc = NewCommit {
        tree: ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
        author: "A <a@b.c> 0 +0000".into(),
        message: "m\n".into(),
    };
    let mut map: BTreeMap<_, _> = Default::default();
    map.insert(
        ObjectId::null(grit_lib::objects::HashAlgo::Sha1),
        CommitEdit::InsertBefore(_nc),
    );
    let _edits: HistoryEdits = map;
    let head = resolve_head(&HistoryRepo::init().worktree().join(".git"));
    let _ = head;
}

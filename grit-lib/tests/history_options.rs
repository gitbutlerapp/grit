//! Shared [`RewriteOptions`] behavior across history entry points.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::refs::write_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, drop as history_drop, reword, squash, ApplyOptions, EmptyPolicy, HistoryError,
    RefScope, RewriteOptions, SquashMessage, SquashOptions,
};

use support::{git_fsck_strict, HistoryRepo};

fn committer(seq: i64) -> String {
    format!("OptsHarness <opts@grit.test> {seq} +0000")
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "OptsHarness <opts@grit.test>".into(),
        reflog_message: "history options".into(),
        record_operation: None,
    }
}

fn write_tree_with_blob(
    repo: &Repository,
    path: &str,
    content: &[u8],
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
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
    repo: &Repository,
    tree: grit_lib::objects::ObjectId,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
    author: &str,
    committer: &str,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    use grit_lib::objects::{serialize_commit, CommitData};
    let raw = serialize_commit(&CommitData {
        tree,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{msg}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    });
    repo.odb().write(ObjectKind::Commit, &raw)
}

fn init_undo_redo_chain(
    grit: &Repository,
) -> (
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
) {
    let tree_a = write_tree_with_blob(grit, "f.txt", b"a\n").expect("tree a");
    let tree_b = write_tree_with_blob(grit, "f.txt", b"b\n").expect("tree b");
    let author = "Author <a@grit.test> 1700000000 +0000";
    let ident = "Writer <w@grit.test> 1700000001 +0000";
    let a = write_test_commit(grit, tree_a, &[], "a", author, ident).expect("a");
    let b = write_test_commit(grit, tree_b, &[a], "b", author, ident).expect("b");
    let c = write_test_commit(grit, tree_a, &[b], "c", author, ident).expect("c");
    write_ref(grit.git_dir(), "refs/heads/main", &c).expect("ref");
    (a, b, c)
}

#[test]
fn drop_keep_empty_keeps_replayed_empty_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let (_a, b, c) = init_undo_redo_chain(&grit);
    let plan = history_drop(
        &grit,
        b,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Keep,
            committer: committer(1).parse().expect("signature"),
            preserve_committer: false,
            sign: true,
        },
    )
    .expect("drop with keep");
    let c_row = plan.commits.iter().find(|r| r.old == c).expect("c row");
    let new_c = c_row.new.expect("empty replay kept as commit");
    assert_ne!(
        new_c, _a,
        "keep must write a new commit, not map to the parent"
    );
    let parsed = parse_commit(&grit.odb().read(&new_c).expect("read").data).expect("parse");
    let parent_tree = write_tree_with_blob(&grit, "f.txt", b"a\n").expect("parent tree");
    assert_eq!(parsed.tree, parent_tree);
    assert_eq!(parsed.parents, vec![_a]);
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn squash_empty_abort_at_head_fast_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let (a, _b, c) = init_undo_redo_chain(&grit);
    let range = format!("{}..{}", a.to_hex(), c.to_hex());
    match squash(
        &grit,
        &[&range],
        &SquashOptions {
            message: SquashMessage::Oldest,
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Abort,
                committer: committer(3).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        },
    ) {
        Err(HistoryError::BecameEmpty { .. }) => {}
        Err(other) => panic!("expected BecameEmpty, got {other:?}"),
        Ok(plan) => panic!("expected error, got plan with {} rows", plan.commits.len()),
    }
}

#[test]
fn squash_keep_committer_at_head_fast_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let tree_a = write_tree_with_blob(&grit, "f.txt", b"a\n").expect("tree a");
    let tree_b = write_tree_with_blob(&grit, "f.txt", b"b\n").expect("tree b");
    let tree_c = write_tree_with_blob(&grit, "f.txt", b"c\n").expect("tree c");
    let author = "Author <a@grit.test> 1700000000 +0000";
    let original_committer = "Original Committer <orig@grit.test> 1700000012 +0000";
    let a = write_test_commit(&grit, tree_a, &[], "a", author, original_committer).expect("a");
    let b = write_test_commit(&grit, tree_b, &[a], "b", author, original_committer).expect("b");
    let c = write_test_commit(&grit, tree_c, &[b], "c", author, original_committer).expect("c");
    write_ref(grit.git_dir(), "refs/heads/main", &c).expect("ref");
    let range = format!("{}..{}", a.to_hex(), c.to_hex());
    let plan = squash(
        &grit,
        &[&range],
        &SquashOptions {
            message: SquashMessage::Oldest,
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer(1800).parse().expect("signature"),
                preserve_committer: true,
                sign: true,
            },
        },
    )
    .expect("squash at head");
    let new_b = plan.replacement(b).expect("squashed");
    let old_raw = grit.odb().read(&b).expect("read old b").data;
    let new_raw = grit.odb().read(&new_b).expect("read new").data;
    let old_line = old_raw
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"committer "))
        .expect("old committer");
    let new_line = new_raw
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"committer "))
        .expect("new committer");
    assert_eq!(new_line, old_line);
    git_fsck_strict(wt);
}

#[test]
fn squash_empty_abort_returns_became_empty() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let (a, b, c) = init_undo_redo_chain(&grit);
    let tree_d = write_tree_with_blob(&grit, "f.txt", b"d\n").expect("tree d");
    let ident = "Writer <w@grit.test> 1700000001 +0000";
    let author = "Author <a@grit.test> 1700000000 +0000";
    let d = write_test_commit(&grit, tree_d, &[c], "d", author, ident).expect("d");
    write_ref(grit.git_dir(), "refs/heads/main", &d).expect("ref");
    let range = format!("{}..{}", a.to_hex(), c.to_hex());
    match squash(
        &grit,
        &[&range],
        &SquashOptions {
            message: SquashMessage::Oldest,
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Abort,
                committer: committer(2).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        },
    ) {
        Err(HistoryError::BecameEmpty { .. }) => {}
        Err(other) => panic!("expected BecameEmpty, got {other:?}"),
        Ok(plan) => panic!("expected error, got plan with {} rows", plan.commits.len()),
    }
}

#[test]
fn reword_keep_committer_preserves_committer_line() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "one.txt", "1\n", "subject");
    let b = support::git_rev_parse(wt, "HEAD").expect("tip");
    let grit = open_grit(wt);
    let plan = reword(
        &grit,
        b,
        "new subject line\n".into(),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(99).parse().expect("signature"),
            preserve_committer: true,
            sign: true,
        },
    )
    .expect("reword");
    let new_b = plan.replacement(b).expect("replacement");
    let old_raw = grit.odb().read(&b).expect("read old").data;
    let new_raw = grit.odb().read(&new_b).expect("read new").data;
    let old_committer = old_raw
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"committer "))
        .expect("old committer line");
    let new_committer = new_raw
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"committer "))
        .expect("new committer line");
    assert_eq!(new_committer, old_committer);
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
}

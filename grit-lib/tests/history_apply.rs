//! Integration tests for [`grit_lib::revwalk::history::apply_rewrite`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::process::Command;

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, plan_rewrite, ApplyOptions, CommitEdit, EmptyPolicy, HistoryEdits, RefScope,
    RewriteOptions,
};

use support::{
    build_commit_dag, commit_file, git, git_init_reftable_repo, git_rev_parse,
    require_reftable_git, HistoryRepo,
};

fn committer(seq: i64) -> String {
    format!("Applier <apply@grit.test> {seq} +0000")
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Applier <apply@grit.test>".into(),
        reflog_message: "history rewrite".into(),
    }
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn tree_plus_file(
    repo: &Repository,
    base_tree: grit_lib::objects::ObjectId,
    path: &str,
    content: &[u8],
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let blob = repo.odb().write(ObjectKind::Blob, content)?;
    let raw = repo.odb().read(&base_tree)?;
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    for entry in parse_tree(&raw.data)? {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        index.add_or_replace(IndexEntry {
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
        });
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

fn plan_reword(
    wt: &std::path::Path,
    target: grit_lib::objects::ObjectId,
    message: &str,
) -> grit_lib::revwalk::history::RewritePlan {
    let grit = open_grit(wt);
    let mut edits = HistoryEdits::new();
    edits.insert(
        target,
        CommitEdit::Rewrite {
            tree: None,
            message: Some(format!("{message}\n")),
            author: None,
            parents: None,
        },
    );
    plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(1),
        },
    )
    .expect("plan")
}

fn plan_reword_with_tree(
    wt: &std::path::Path,
    target: grit_lib::objects::ObjectId,
    path: &str,
    content: &[u8],
    message: &str,
) -> grit_lib::revwalk::history::RewritePlan {
    let grit = open_grit(wt);
    let parent_tree = parse_commit(&grit.odb().read(&target).expect("read").data)
        .expect("parse")
        .tree;
    let new_tree = tree_plus_file(&grit, parent_tree, path, content).expect("tree");
    let mut edits = HistoryEdits::new();
    edits.insert(
        target,
        CommitEdit::Rewrite {
            tree: Some(new_tree),
            message: Some(format!("{message}\n")),
            author: None,
            parents: None,
        },
    );
    plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(1),
        },
    )
    .expect("plan")
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

fn git_porcelain(wt: &std::path::Path) -> String {
    String::from_utf8(
        Command::new("git")
            .current_dir(wt)
            .args(["status", "--porcelain"])
            .output()
            .expect("status")
            .stdout,
    )
    .expect("utf8")
}

#[test]
fn apply_rewrite_updates_refs_atomically_files_backend() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reworded");
    let grit = open_grit(wt);
    let applied = apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    assert_eq!(applied.ref_updates.len(), 1);
    let new_tip = applied.head;
    assert_ne!(new_tip, oids["c"]);
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/main").expect("main"),
        new_tip
    );
    assert_ne!(new_tip, oids["c"]);
    git_fsck_strict(wt);
}

#[test]
fn apply_rewrite_updates_refs_atomically_reftable_backend() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let wt = root.path();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reftable reword");
    let grit = open_grit(wt);
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn apply_rewrite_cas_mismatch_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reworded");
    commit_file(wt, "move.txt", "tip\n", "advance tip");
    let tip_after_move = git_rev_parse(wt, "refs/heads/main").expect("tip");
    let grit = open_grit(wt);
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("cas");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::RefCasMismatch { .. }
    ));
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), Some(tip_after_move));
}

#[test]
fn apply_rewrite_refuses_when_local_changes_would_be_overwritten() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword_with_tree(wt, oids["b"], "clash.txt", b"from rewrite\n", "tree change");
    fs::write(wt.join("clash.txt"), "local untracked\n").expect("write");
    let grit = open_grit(wt);
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("blocked");
    match err {
        grit_lib::revwalk::history::HistoryError::LocalChangesWouldBeOverwritten { paths } => {
            assert!(paths.iter().any(|p| p.contains("clash")));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), Some(oids["c"]));
}

#[test]
fn apply_rewrite_preserves_unrelated_staged_and_unstaged_changes() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    commit_file(wt, "keep.txt", "base\n", "add keep");
    let plan = plan_reword_with_tree(wt, oids["b"], "rewritten-only.txt", b"new\n", "tree change");
    fs::write(wt.join("keep.txt"), "unstaged edit\n").expect("unstaged");
    fs::write(wt.join("staged.txt"), "new\n").expect("staged");
    git(wt, &["add", "staged.txt"]);
    let grit = open_grit(wt);
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    assert_eq!(
        fs::read_to_string(wt.join("keep.txt")).expect("read"),
        "unstaged edit\n"
    );
    assert_eq!(
        fs::read_to_string(wt.join("staged.txt")).expect("read"),
        "new\n"
    );
    git_fsck_strict(wt);
}

#[test]
fn apply_rewrite_bare_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(
        tmp.path(),
        &["init", "--bare", "-b", "main", tmp.path().to_str().unwrap()],
    );
    let grit = Repository::discover(
        Some(tmp.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open bare");
    let empty = grit.odb().write(ObjectKind::Tree, &[]).expect("tree");
    let commit = grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData {
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
        .write(ObjectKind::Commit, &commit)
        .expect("commit");
    grit_lib::refs::write_ref(grit.git_dir(), "refs/heads/main", &oid).expect("ref");
    let mut edits = HistoryEdits::new();
    edits.insert(
        oid,
        CommitEdit::Rewrite {
            tree: None,
            message: Some("bare reword\n".into()),
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
            committer: committer(2),
        },
    )
    .expect("plan");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply bare");
}

#[test]
fn apply_rewrite_writes_reflog_entries() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reworded");
    let grit = open_grit(wt);
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let reflog = git(wt, &["reflog", "show", "-1", "main"]);
    assert!(
        reflog.contains("history rewrite"),
        "reflog must record apply message: {reflog}"
    );
}

#[test]
fn ref_update_lines_round_trip_through_git_update_ref_stdin() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "stdin roundtrip");
    let lines = plan.ref_update_lines();
    assert_eq!(lines.len(), 1);
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    use std::io::Write;
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in &lines {
            writeln!(stdin, "{line}").expect("write");
        }
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git update-ref --stdin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git_rev_parse(wt, "refs/heads/main"),
        Some(plan.ref_updates[0].new)
    );
    git_fsck_strict(wt);
}

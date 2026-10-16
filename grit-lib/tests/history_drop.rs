//! Tests for [`grit_lib::history::drop`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::process::Command;

use grit_lib::history::{
    apply_rewrite, drop as history_drop, ApplyOptions, EmptyPolicy, HistoryError, RefScope,
    RewriteOptions,
};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::Repository;

use support::{build_commit_dag, commit_file, git, git_fsck_strict, git_rev_parse, HistoryRepo};

fn committer(seq: i64) -> String {
    format!("Dropper <drop@grit.test> {seq} +0000")
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn opts(seq: i64) -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(seq),
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Dropper <drop@grit.test>".into(),
        reflog_message: "history drop".into(),
    }
}

fn write_tree_with_blob(
    repo: &Repository,
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
    repo: &Repository,
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

#[test]
fn drop_replays_descendants_onto_parent() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let plan = history_drop(&grit, oids["b"], &opts(1)).expect("drop");
    let dropped = plan
        .commits
        .iter()
        .find(|r| r.old == oids["b"])
        .expect("b row");
    assert!(dropped.new.is_none());
    let new_c = plan
        .commits
        .iter()
        .find(|r| r.old == oids["c"])
        .and_then(|r| r.new)
        .expect("new c");
    let parsed = parse_commit(&grit.odb().read(&new_c).expect("read").data).expect("parse");
    assert_eq!(parsed.parents, vec![oids["a"]]);
    git_fsck_strict(wt);
}

#[test]
fn drop_moves_branch_at_dropped_commit_to_parent() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    git(
        wt,
        &["update-ref", "refs/heads/points-at-b", &oids["b"].to_hex()],
    );
    let grit = open_grit(wt);
    let plan = history_drop(&grit, oids["b"], &opts(2)).expect("drop");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let tip = git_rev_parse(wt, "refs/heads/points-at-b").expect("ref");
    assert_eq!(tip, oids["a"]);
    git_fsck_strict(wt);
}

#[test]
fn drop_rejects_root_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = history_drop(&grit, oids["a"], &opts(3)).unwrap_err();
    assert_eq!(err, HistoryError::RootCommit { commit: oids["a"] });
}

#[test]
fn drop_rejects_merge_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "a"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let err = history_drop(&grit, oids["m"], &opts(4)).unwrap_err();
    assert_eq!(err, HistoryError::MergeCommitInReplay { commit: oids["m"] });
}

#[test]
fn drop_conflict_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit = Repository::init(
        dir.path(),
        &grit_lib::repo::InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let ident = committer(5);
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
    let before = resolve_ref(grit.git_dir(), "refs/heads/main").expect("resolve");
    let err = history_drop(&grit, b, &opts(5)).expect_err("conflict");
    assert!(matches!(err, HistoryError::Conflict { .. }));
    let after = resolve_ref(grit.git_dir(), "refs/heads/main").expect("resolve");
    assert_eq!(before, after);
}

#[test]
fn drop_syncs_worktree_when_head_affected() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "first.t", "1\n", "first");
    let a = git_rev_parse(wt, "HEAD").expect("a");
    commit_file(wt, "second.t", "2\n", "second");
    let b = git_rev_parse(wt, "HEAD").expect("b");
    commit_file(wt, "third.t", "3\n", "third");
    let grit = open_grit(wt);
    let plan = history_drop(&grit, b, &opts(6)).expect("drop");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    assert!(wt.join("first.t").is_file());
    assert!(wt.join("third.t").is_file());
    assert!(
        !wt.join("second.t").exists(),
        "dropped commit's file must leave the worktree"
    );
    assert_ne!(git_rev_parse(wt, "HEAD"), Some(b));
    let _ = a;
    git_fsck_strict(wt);
}

#[test]
fn history_drop_matches_git_rebase_drop() {
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let git_dir = tempfile::tempdir().expect("git dir");
    git(grit_dir.path(), &["init", "-b", "main"]);
    commit_file(grit_dir.path(), "first.t", "1\n", "first");
    commit_file(grit_dir.path(), "second.t", "2\n", "second");
    commit_file(grit_dir.path(), "third.t", "3\n", "third");
    git(
        grit_dir.path(),
        &[
            "clone",
            grit_dir.path().to_str().expect("path"),
            git_dir.path().to_str().expect("git path"),
        ],
    );
    let b = git(grit_dir.path(), &["rev-parse", "HEAD~"])
        .trim()
        .to_string();
    let replay_committer = "Test User <test@example.com> 1700000002 +0000".to_string();
    let grit = open_grit(grit_dir.path());
    let plan = history_drop(
        &grit,
        b.parse().expect("oid"),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: replay_committer,
        },
    )
    .expect("plan");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let editor = git_dir.path().join("rebase-drop.sh");
    fs::write(&editor, "#!/bin/sh\nsed -i '2s/^pick/drop/' \"$1\"\n").expect("write editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let rebase = Command::new("git")
        .current_dir(git_dir.path())
        .args(["rebase", "-i", "--root"])
        .env("GIT_SEQUENCE_EDITOR", editor.to_string_lossy().as_ref())
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rebase");
    assert!(
        rebase.status.success(),
        "git rebase -i drop failed: {}",
        String::from_utf8_lossy(&rebase.stderr)
    );
    let grit_tip = git(grit_dir.path(), &["rev-parse", "main"])
        .trim()
        .to_string();
    let git_tip = git(git_dir.path(), &["rev-parse", "main"])
        .trim()
        .to_string();
    assert_eq!(grit_tip, git_tip);
    let grit_log = git(grit_dir.path(), &["rev-list", "--reverse", "main"]);
    let git_log = git(git_dir.path(), &["rev-list", "--reverse", "main"]);
    assert_eq!(grit_log, git_log);
    git_fsck_strict(grit_dir.path());
    git_fsck_strict(git_dir.path());
}

#[test]
fn history_drop_matches_git_history_when_available() {
    if !support::git_version_at_least(2, 54) {
        eprintln!("skipping: system git < 2.54 (no git history drop)");
        return;
    }
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let git_dir = tempfile::tempdir().expect("git dir");
    for dir in [grit_dir.path(), git_dir.path()] {
        git(dir, &["init", "-b", "main"]);
        commit_file(dir, "first.t", "1\n", "first");
        commit_file(dir, "second.t", "2\n", "second");
        commit_file(dir, "third.t", "3\n", "third");
    }
    let b = git(grit_dir.path(), &["rev-parse", "HEAD~"])
        .trim()
        .to_string();
    let grit = open_grit(grit_dir.path());
    let plan = history_drop(
        &grit,
        b.parse().expect("oid"),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(8),
        },
    )
    .expect("plan");
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let git_out = Command::new("git")
        .current_dir(git_dir.path())
        .args(["history", "drop", "HEAD~"])
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git history drop");
    assert!(
        git_out.status.success(),
        "git history drop: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let grit_tip = git(grit_dir.path(), &["rev-parse", "main"])
        .trim()
        .to_string();
    let git_tip = git(git_dir.path(), &["rev-parse", "main"])
        .trim()
        .to_string();
    assert_eq!(grit_tip, git_tip);
    git_fsck_strict(grit_dir.path());
    git_fsck_strict(git_dir.path());
}

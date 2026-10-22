//! Tests for [`grit_lib::revwalk::history::split`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::process::Command;

use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    split, EmptyPolicy, HistoryError, RefScope, RewriteOptions, SplitOptions,
};

use support::{build_commit_dag, commit_file, git, git_fsck_strict, HistoryRepo};

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

fn split_plan(
    grit: &Repository,
    commit: ObjectId,
    pathspec: &[&str],
    split_message: &str,
    rest_message: Option<&str>,
    committer_line: &str,
) -> grit_lib::revwalk::history::RewritePlan {
    let specs: Vec<String> = pathspec.iter().map(|s| (*s).to_string()).collect();
    let opts = SplitOptions {
        rest_message: rest_message.map(str::to_string),
    };
    split(
        grit,
        commit,
        &specs,
        split_message.to_string(),
        &opts,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer_line.to_string(),
            preserve_committer: false,
            sign: true,
        },
    )
    .expect("split plan")
}

#[test]
fn split_moves_matching_paths_into_new_parent() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "initial.t", "x", "initial");
    std::fs::write(wt.join("bar"), b"").unwrap();
    std::fs::write(wt.join("foo"), b"").unwrap();
    git(wt, &["add", "bar", "foo"]);
    git(wt, &["commit", "-m", "split-me"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["foo"],
        "first\n",
        Some("second\n"),
        &committer(1),
    );

    let head_row = plan
        .commits
        .iter()
        .find(|r| r.old == head)
        .expect("head row");
    let new_head = head_row.new.expect("new head");
    let new_head_parsed = parse_commit(&grit.odb().read(&new_head).unwrap().data).unwrap();
    let split_commit = *new_head_parsed.parents.first().expect("split parent");
    let split_tree = parse_commit(&grit.odb().read(&split_commit).unwrap().data)
        .unwrap()
        .tree;
    let head_tree = parse_commit(&grit.odb().read(&new_head).unwrap().data)
        .unwrap()
        .tree;

    assert!(tree_has_path(&grit, split_tree, "foo"));
    assert!(!tree_has_path(&grit, split_tree, "bar"));
    assert!(tree_has_path(&grit, head_tree, "bar"));
    assert!(tree_has_path(&grit, head_tree, "foo"));
    git_fsck_strict(wt);
}

#[test]
fn split_original_keeps_tree_and_author() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    std::fs::write(wt.join("picked.txt"), b"v").unwrap();
    std::fs::write(wt.join("other.txt"), b"w").unwrap();
    git(wt, &["add", "picked.txt", "other.txt"]);
    git(wt, &["commit", "-m", "add files"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["picked.txt"],
        "split side\n",
        None,
        &committer(2),
    );
    let row = plan.commits.iter().find(|r| r.old == head).unwrap();
    let new_oid = row.new.unwrap();
    let old = parse_commit(&grit.odb().read(&head).unwrap().data).unwrap();
    let new = parse_commit(&grit.odb().read(&new_oid).unwrap().data).unwrap();
    assert_eq!(old.tree, new.tree);
    assert_eq!(old.author, new.author);
    assert_eq!(old.message, new.message);
    git_fsck_strict(wt);
}

#[test]
fn split_keep_committer_preserves_source_committer_line() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "initial.t", "x", "initial");
    std::fs::write(wt.join("bar"), b"").unwrap();
    std::fs::write(wt.join("foo"), b"").unwrap();
    git(wt, &["add", "bar", "foo"]);
    git(wt, &["commit", "-m", "split-me"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let grit = open_grit(wt);
    let plan = split(
        &grit,
        head,
        &["foo".to_string()],
        "split side\n".into(),
        &SplitOptions::default(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(999),
            preserve_committer: true,
            sign: true,
        },
    )
    .expect("split");
    let new_head = plan.replacement(head).expect("rest commit");
    let old_raw = grit.odb().read(&head).expect("read old").data;
    let new_raw = grit.odb().read(&new_head).expect("read new").data;
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
fn split_rejects_all_or_nothing_selection() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "initial.t", "x", "initial");
    std::fs::write(wt.join("bar"), b"").unwrap();
    std::fs::write(wt.join("foo"), b"").unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-m", "split-me"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let grit = open_grit(wt);

    let err = split(
        &grit,
        head,
        &["nonexistent".to_string()],
        "x\n".into(),
        &SplitOptions::default(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(3),
            preserve_committer: false,
            sign: true,
        },
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::SplitSelectsNothing));

    let err = split(
        &grit,
        head,
        &["foo".to_string(), "bar".to_string()],
        "x\n".into(),
        &SplitOptions::default(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(4),
            preserve_committer: false,
            sign: true,
        },
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::SplitSelectsEverything));
}

#[test]
fn split_rejects_merge_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "a"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let err = split(
        &grit,
        oids["m"],
        &["any".to_string()],
        "split\n".into(),
        &SplitOptions::default(),
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: committer(5),
            preserve_committer: false,
            sign: true,
        },
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::InsertBeforeOnMerge { .. }));
}

#[test]
fn split_in_bare_repository() {
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
    let empty = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Tree, &[])
        .expect("tree");
    let root = grit
        .odb()
        .write(
            grit_lib::objects::ObjectKind::Commit,
            &grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData {
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
            }),
        )
        .expect("root commit");
    grit_lib::refs::write_ref(grit.git_dir(), "refs/heads/main", &root).expect("ref");
    let blob_f = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"x")
        .expect("blob");
    let blob_g = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"y")
        .expect("blob");
    let tree_bytes = grit_lib::objects::serialize_tree(&[
        grit_lib::objects::TreeEntry {
            mode: 0o100644,
            name: b"f".to_vec(),
            oid: blob_f,
        },
        grit_lib::objects::TreeEntry {
            mode: 0o100644,
            name: b"g".to_vec(),
            oid: blob_g,
        },
    ]);
    let child_tree = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Tree, &tree_bytes)
        .expect("tree");
    let tip = grit
        .odb()
        .write(
            grit_lib::objects::ObjectKind::Commit,
            &grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData {
                tree: child_tree,
                parents: vec![root],
                author: committer(1),
                committer: committer(1),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "tip\n".into(),
                raw_message: None,
                extra_headers: vec![],
            }),
        )
        .expect("tip");
    grit_lib::refs::write_ref(grit.git_dir(), "refs/heads/main", &tip).expect("update tip");
    let plan = split(
        &grit,
        tip,
        &["f".to_string()],
        "split side\n".into(),
        &SplitOptions::default(),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: committer(2),
            preserve_committer: false,
            sign: true,
        },
    )
    .expect("bare split");
    assert!(plan.commits.iter().any(|r| r.old == tip && r.new.is_some()));
}

#[test]
fn split_descendants_keep_trees() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "base.t", "b", "base");
    std::fs::write(wt.join("move.me"), b"1").unwrap();
    std::fs::write(wt.join("stay.me"), b"2").unwrap();
    git(wt, &["add", "move.me", "stay.me"]);
    git(wt, &["commit", "-m", "split target"]);
    let split_target: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    commit_file(wt, "child.t", "c", "child");
    let tip: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        split_target,
        &["move.me"],
        "peeled\n",
        None,
        &committer(8),
    );
    let child_row = plan.commits.iter().find(|r| r.old == tip).unwrap();
    let new_child = child_row.new.unwrap();
    let old_child_tree = parse_commit(&grit.odb().read(&tip).unwrap().data)
        .unwrap()
        .tree;
    let new_child_tree = parse_commit(&grit.odb().read(&new_child).unwrap().data)
        .unwrap()
        .tree;
    assert_eq!(old_child_tree, new_child_tree);
    git_fsck_strict(wt);
}

#[test]
fn split_rename_only_commit_new_pathspec_succeeds() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "old.txt", "v", "add old");
    git(wt, &["mv", "old.txt", "new.txt"]);
    git(wt, &["commit", "-m", "rename only"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["new.txt"],
        "split new\n",
        None,
        &committer(9),
    );
    let new_head = plan
        .commits
        .iter()
        .find(|r| r.old == head)
        .and_then(|r| r.new)
        .expect("new head");
    let parent = parse_commit(&grit.odb().read(&new_head).unwrap().data)
        .unwrap()
        .parents[0];
    let split_tree = parse_commit(&grit.odb().read(&parent).unwrap().data)
        .unwrap()
        .tree;
    assert!(tree_has_path(&grit, split_tree, "old.txt"));
    assert!(tree_has_path(&grit, split_tree, "new.txt"));
    git_fsck_strict(wt);
}

#[test]
fn split_rename_pathspec_new_side_keeps_old_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "old.txt", "v", "add old");
    git(wt, &["mv", "old.txt", "new.txt"]);
    std::fs::write(wt.join("other.txt"), b"x").unwrap();
    git(wt, &["add", "other.txt"]);
    git(wt, &["commit", "-m", "rename and add"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["new.txt"],
        "split new\n",
        None,
        &committer(10),
    );
    let new_head = plan
        .commits
        .iter()
        .find(|r| r.old == head)
        .and_then(|r| r.new)
        .expect("new head");
    let parent = parse_commit(&grit.odb().read(&new_head).unwrap().data)
        .unwrap()
        .parents[0];
    let split_tree = parse_commit(&grit.odb().read(&parent).unwrap().data)
        .unwrap()
        .tree;
    assert!(tree_has_path(&grit, split_tree, "old.txt"));
    assert!(tree_has_path(&grit, split_tree, "new.txt"));
    git_fsck_strict(wt);
}

#[test]
fn split_copy_pathspec_new_side_keeps_source_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "source.txt", "v", "add source");
    std::fs::write(wt.join("copy.txt"), b"v").unwrap();
    std::fs::write(wt.join("other.txt"), b"x").unwrap();
    git(wt, &["add", "copy.txt", "other.txt"]);
    git(wt, &["commit", "-m", "copy and add"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["copy.txt"],
        "split copy\n",
        None,
        &committer(12),
    );
    let new_head = plan
        .commits
        .iter()
        .find(|r| r.old == head)
        .and_then(|r| r.new)
        .expect("new head");
    let parent = parse_commit(&grit.odb().read(&new_head).unwrap().data)
        .unwrap()
        .parents[0];
    let split_tree = parse_commit(&grit.odb().read(&parent).unwrap().data)
        .unwrap()
        .tree;
    assert!(tree_has_path(&grit, split_tree, "source.txt"));
    assert!(tree_has_path(&grit, split_tree, "copy.txt"));
    git_fsck_strict(wt);
}

#[test]
fn split_file_to_directory_partial_add_avoids_df_clash() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "foo", "file", "file at foo");
    std::fs::remove_file(wt.join("foo")).unwrap();
    std::fs::create_dir_all(wt.join("foo")).unwrap();
    std::fs::write(wt.join("foo/bar"), b"nested").unwrap();
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "file to dir"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();

    let grit = open_grit(wt);
    let plan = split_plan(
        &grit,
        head,
        &["foo/bar"],
        "nested only\n",
        None,
        &committer(11),
    );
    let new_head = plan
        .commits
        .iter()
        .find(|r| r.old == head)
        .and_then(|r| r.new)
        .expect("new head");
    let parent = parse_commit(&grit.odb().read(&new_head).unwrap().data)
        .unwrap()
        .parents[0];
    let split_tree = parse_commit(&grit.odb().read(&parent).unwrap().data)
        .unwrap()
        .tree;
    assert!(tree_has_path(&grit, split_tree, "foo/bar"));
    git_fsck_strict(wt);
}

#[test]
fn history_split_matches_git_commit_tree_reconstruction() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let _ = commit_file(wt, "initial.t", "x", "initial");
    std::fs::write(wt.join("bar"), b"").unwrap();
    std::fs::write(wt.join("foo"), b"").unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-m", "split-me"]);
    let head: ObjectId = git(wt, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let grit = open_grit(wt);
    let committer_line = committer(99);
    let plan = split_plan(
        &grit,
        head,
        &["foo"],
        "first\n",
        Some("second\n"),
        &committer_line,
    );

    for row in &plan.commits {
        let Some(new_oid) = row.new else {
            continue;
        };
        let raw = grit.odb().read(&new_oid).expect("read").data;
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
            new_oid.to_hex(),
            git_oid,
            "written commit must round-trip through git with identical oid"
        );
    }
    git_fsck_strict(wt);
}

fn tree_has_path(grit: &Repository, tree: ObjectId, path: &str) -> bool {
    let mut current = tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let obj = grit.odb().read(&current).expect("read tree");
        let entries = grit_lib::objects::parse_tree(&obj.data).expect("parse");
        let Some(entry) = entries.iter().find(|e| e.name == part.as_bytes()) else {
            return false;
        };
        if i + 1 == parts.len() {
            return entry.mode != grit_lib::index::MODE_TREE;
        }
        if entry.mode != grit_lib::index::MODE_TREE {
            return false;
        }
        current = entry.oid;
    }
    false
}

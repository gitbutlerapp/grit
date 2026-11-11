//! Integration tests for [`grit_lib::revwalk::history::apply_rewrite`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::process::Command;

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
use grit_lib::progress::CancelToken;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    ApplyOptions, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
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
        identity: support::reflog_signature("Applier <apply@grit.test>"),
        reflog_message: "history rewrite".into(),
        record_operation: None,
        hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
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
    write_tree_from_index(repo.odb(), &index, "").map_err(Into::into)
}

fn tree_replace_file(
    repo: &Repository,
    base_tree: grit_lib::objects::ObjectId,
    path: &str,
    content: &[u8],
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let blob = repo.odb().write(ObjectKind::Blob, content)?;
    let raw = repo.odb().read(&base_tree)?;
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    let path_bytes = path.as_bytes();
    for entry in parse_tree(&raw.data)? {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        if name == path {
            continue;
        }
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
        path: path_bytes.to_vec(),
        base_index_pos: 0,
    });
    write_tree_from_index(repo.odb(), &index, "").map_err(Into::into)
}

fn plan_reword_tree_from_commit(
    wt: &std::path::Path,
    target: grit_lib::objects::ObjectId,
    path: &str,
    content: &[u8],
    message: &str,
) -> grit_lib::revwalk::history::RewritePlan {
    let grit = open_grit(wt);
    let tree = parse_commit(&grit.odb().read(&target).expect("read").data)
        .expect("parse")
        .tree;
    let new_tree = tree_replace_file(&grit, tree, path, content).expect("tree");
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
    grit.history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan")
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
    grit.history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
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
    grit.history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
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
    let applied = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    assert_eq!(applied.ref_updates.len(), 1);
    let new_tip = applied.head;
    assert_ne!(new_tip, oids["c"]);
    assert_eq!(
        grit.references().resolve("refs/heads/main").expect("main"),
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
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn apply_rewrite_refuses_when_head_moved_since_plan() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    git(wt, &["update-ref", "refs/heads/other", &oids["c"].to_hex()]);
    let plan = {
        let grit = open_grit(wt);
        let mut edits = HistoryEdits::new();
        let parent_tree = parse_commit(&grit.odb().read(&oids["b"]).expect("read").data)
            .expect("parse")
            .tree;
        let new_tree =
            tree_plus_file(&grit, parent_tree, "rewritten-only.txt", b"new\n").expect("tree");
        edits.insert(
            oids["b"],
            CommitEdit::Rewrite {
                tree: Some(new_tree),
                message: Some("tree change\n".into()),
                author: None,
                parents: None,
            },
        );
        grit.history()
            .plan_rewrite(
                &edits,
                &RewriteOptions {
                    scope: RefScope::Head,
                    empty: EmptyPolicy::Drop,
                    committer: committer(1).parse().expect("signature"),
                    preserve_committer: false,
                    sign: true,
                },
            )
            .expect("plan")
    };
    git(wt, &["switch", "other"]);
    let main_before = git_rev_parse(wt, "refs/heads/main").expect("main");
    let grit = open_grit(wt);
    let err = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("head moved");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::HeadMovedSincePlan { .. }
    ));
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), Some(main_before));
    assert!(
        git_porcelain(wt).trim().is_empty(),
        "other branch worktree must be untouched: {}",
        git_porcelain(wt)
    );
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
    let err = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("stale plan");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::HeadMovedSincePlan { .. }
            | grit_lib::revwalk::history::HistoryError::RefCasMismatch { .. }
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
    let err = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("blocked");
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
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
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
    let commit = grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData::new(
        empty,
        vec![],
        committer(0),
        committer(0),
        "root\n",
    ));
    let oid = grit
        .odb()
        .write(ObjectKind::Commit, &commit)
        .expect("commit");
    grit_test_support::refs::set_ref(&grit, "refs/heads/main", &oid).expect("ref");
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
    let plan = grit
        .history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer(2).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply bare");
}

#[test]
fn apply_rewrite_writes_reflog_entries() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reworded");
    let grit = open_grit(wt);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let tip = grit.references().resolve("refs/heads/main").expect("main");
    let branch_reflog = git(wt, &["reflog", "show", "-1", "main"]);
    assert!(
        branch_reflog.contains("history rewrite"),
        "reflog must record apply message: {branch_reflog}"
    );
    let head_reflog = git(wt, &["reflog", "show", "-1", "HEAD"]);
    assert!(
        head_reflog.contains("history rewrite"),
        "HEAD reflog must record apply message: {head_reflog}"
    );
    let head_log = grit.references().reflog("HEAD").expect("parse HEAD reflog");
    let branch_log = grit
        .references()
        .reflog("refs/heads/main")
        .expect("parse branch reflog");
    let head_entry = head_log.last().expect("HEAD reflog entry");
    let branch_entry = branch_log.last().expect("branch reflog entry");
    assert_eq!(head_entry.new_oid, tip);
    assert_eq!(head_entry.new_oid, branch_entry.new_oid);
    assert_eq!(head_entry.old_oid, branch_entry.old_oid);
    assert_eq!(head_entry.message, branch_entry.message);
}

#[test]
fn apply_rewrite_checked_out_branch_reflog_single_entry_per_update() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let mut tips = Vec::new();
    for i in 1..=3 {
        tips.push(commit_file(
            wt,
            &format!("f{i}"),
            &i.to_string(),
            &format!("c{i}"),
        ));
    }
    let pre_rewrite_tip = tips[2];
    let plan = plan_reword(wt, tips[1], "reworded");
    let grit = open_grit(wt);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let new_tip = grit
        .references()
        .resolve("refs/heads/main")
        .expect("main tip");
    let head_log = grit.references().reflog("HEAD").expect("HEAD reflog");
    let branch_log = grit
        .references()
        .reflog("refs/heads/main")
        .expect("main reflog");
    assert_eq!(
        head_log.len(),
        4,
        "HEAD reflog: three commits plus one history rewrite"
    );
    assert_eq!(
        branch_log.len(),
        4,
        "branch reflog: three commits plus one history rewrite"
    );
    let head_entry = head_log.last().expect("last HEAD entry");
    let branch_entry = branch_log.last().expect("last branch entry");
    assert_eq!(head_entry.old_oid, pre_rewrite_tip);
    assert_eq!(head_entry.new_oid, new_tip);
    assert_eq!(head_entry.old_oid, branch_entry.old_oid);
    assert_eq!(head_entry.new_oid, branch_entry.new_oid);
    let prev = branch_log
        .get(branch_log.len() - 2)
        .expect("previous branch entry");
    assert!(
        branch_entry.old_oid != prev.old_oid || branch_entry.new_oid != prev.new_oid,
        "branch reflog must not append the same old->new pair twice"
    );
    let head_at_1 = git(wt, &["rev-parse", "HEAD@{1}"]).trim().to_string();
    let main_at_1 = git(wt, &["rev-parse", "main@{1}"]).trim().to_string();
    assert_eq!(
        head_at_1,
        pre_rewrite_tip.to_hex(),
        "system git HEAD@{{1}} must be pre-rewrite branch tip"
    );
    assert_eq!(
        main_at_1,
        pre_rewrite_tip.to_hex(),
        "system git main@{{1}} must be pre-rewrite branch tip"
    );
}

#[test]
fn apply_rewrite_head_reflog_matches_git_update_ref_on_checked_out_branch() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let plan = plan_reword(wt, oids["b"], "reworded");
    let grit = open_grit(wt);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let tip = grit.references().resolve("refs/heads/main").expect("main");
    let head_gs = git(wt, &["reflog", "show", "-1", "--format=%gs", "HEAD"]);
    let branch_gs = git(
        wt,
        &["reflog", "show", "-1", "--format=%gs", "refs/heads/main"],
    );
    assert_eq!(
        head_gs, branch_gs,
        "HEAD and branch reflog messages must match"
    );
    let head_old_new = git(wt, &["reflog", "show", "-1", "--format=%H %gs", "HEAD"]);
    assert!(
        head_old_new.contains(&tip.to_hex()),
        "HEAD tip reflog entry must point at rewritten tip: {head_old_new}"
    );
}

#[test]
fn apply_rewrite_head_reflog_reftable_backend() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let wt = root.path();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "reftable reword");
    let grit = open_grit(wt);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let head_log = grit.references().reflog("HEAD").expect("head log");
    let branch_log = grit
        .references()
        .reflog("refs/heads/main")
        .expect("branch log");
    let head_entry = head_log.last().expect("HEAD reflog entry");
    let branch_entry = branch_log.last().expect("branch reflog entry");
    assert_eq!(head_entry.new_oid, branch_entry.new_oid);
    assert_eq!(head_entry.old_oid, branch_entry.old_oid);
    assert_eq!(head_entry.message, branch_entry.message);
    git_fsck_strict(wt);
}

#[test]
fn ref_update_lines_round_trip_through_git_update_ref_stdin() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let plan = plan_reword(wt, oids["b"], "stdin roundtrip");
    let lines: Vec<String> = plan
        .ref_updates
        .iter()
        .map(|u| format!("update {} {} {}", u.name, u.new.to_hex(), u.old.to_hex()))
        .collect();
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

#[test]
fn apply_rewrite_failed_checkout_restores_refs_and_preserves_dirty_worktree() {
    use grit_lib::test_support::inject::set_test_inject_index_write_fail_remaining;

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    commit_file(wt, "local.txt", "base\n", "add local");
    let grit = open_grit(wt);
    let staged_oid = {
        use std::io::Write;
        let mut child = Command::new("git")
            .current_dir(wt)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("hash-object");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"staged\n")
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(
        wt,
        &[
            "update-index",
            "--cacheinfo",
            "100644",
            &staged_oid,
            "local.txt",
        ],
    );
    fs::write(wt.join("local.txt"), "dirty\n").expect("dirty worktree");

    let plan = plan_reword_with_tree(
        wt,
        oids["b"],
        "rewritten-only.txt",
        b"from rewrite\n",
        "tree change",
    );
    let tip_before = git_rev_parse(wt, "refs/heads/main").expect("tip");
    set_test_inject_index_write_fail_remaining(1);
    let err = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected checkout failure");
    grit_lib::test_support::inject::set_test_inject_index_write_fail(false);
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::WorktreeApply { .. }
    ));
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), Some(tip_before));
    assert_eq!(
        fs::read_to_string(wt.join("local.txt")).expect("read"),
        "dirty\n",
        "unstaged worktree content must survive rollback"
    );
    let index_line = git(wt, &["ls-files", "-s", "local.txt"]);
    assert!(
        index_line.contains(&staged_oid),
        "index must still record staged blob, got: {index_line}"
    );
    assert!(
        !wt.join("rewritten-only.txt").exists(),
        "partial checkout paths must be rolled back"
    );
}

#[test]
fn interrupted_rewrite_apply_recovers_and_blocks_commit_until_finished() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let plan = plan_reword_with_tree(
        wt,
        oids["b"],
        "rewritten-only.txt",
        b"from rewrite\n",
        "tree change",
    );
    assert_ne!(plan.head.old_tree, plan.head.new_tree);
    let tip_before = git_rev_parse(wt, "refs/heads/main").expect("tip");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    let err = grit
        .history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort after refs");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    let grit_lib::revwalk::history::HistoryError::WorktreeApply { source } = err else {
        panic!("unexpected apply error");
    };
    let message = source.to_string();
    assert!(
        message.contains("injected abort after rewrite ref updates"),
        "unexpected message: {message}"
    );
    assert!(grit.history().apply_journal_active());
    assert_ne!(git_rev_parse(wt, "refs/heads/main"), Some(tip_before));
    assert!(wt.join("rewritten-only.txt").exists() == false);

    let recovery = grit
        .history()
        .recover_interrupted_apply()
        .expect("recover")
        .expect("recovery outcome");
    assert_eq!(
        recovery,
        grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout
    );
    assert!(!grit.history().apply_journal_active());
    assert!(wt.join("rewritten-only.txt").exists());
}

/// Plan a rewrite of `tip` whose tree replaces (or adds) each top-level `(path, content)`.
fn plan_tip_tree_edits(
    wt: &std::path::Path,
    tip: grit_lib::objects::ObjectId,
    files: &[(&str, &[u8])],
) -> grit_lib::revwalk::history::RewritePlan {
    let grit = open_grit(wt);
    let mut tree = parse_commit(&grit.odb().read(&tip).expect("read").data)
        .expect("parse")
        .tree;
    for (path, content) in files {
        tree = tree_replace_file(&grit, tree, path, content).expect("tree");
    }
    let mut edits = HistoryEdits::new();
    edits.insert(
        tip,
        CommitEdit::Rewrite {
            tree: Some(tree),
            message: Some("rewrite many paths\n".into()),
            author: None,
            parents: None,
        },
    );
    grit.history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan")
}

/// Commit three tracked files plus an unrelated one, dirty the unrelated file and add an
/// untracked one, then stop a rewrite of every tracked file right after its ref updates and
/// leave a half-written checkout behind (one path finished, one truncated, one new file).
fn interrupted_partial_checkout() -> (HistoryRepo, grit_lib::revwalk::history::RewritePlan) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    commit_file(wt, "one.txt", "one old\n", "one");
    commit_file(wt, "two.txt", "two old\n", "two");
    let tip = commit_file(wt, "local.txt", "local base\n", "local");
    let plan = plan_tip_tree_edits(
        wt,
        tip,
        &[
            ("one.txt", b"one new\n"),
            ("two.txt", b"two new, a much longer line\n"),
            ("added.txt", b"added\n"),
        ],
    );
    fs::write(wt.join("local.txt"), "local edit, longer\n").expect("dirty unrelated file");
    fs::write(wt.join("scratch.txt"), "scratch\n").expect("untracked file");

    let grit = open_grit(wt);
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort after refs");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    assert!(grit.history().apply_journal_active());

    fs::write(wt.join("one.txt"), "one new\n").expect("finished path");
    fs::write(wt.join("two.txt"), "two n").expect("truncated path");
    fs::write(wt.join("added.txt"), "added\n").expect("new path");
    (repo, plan)
}

fn assert_finished_partial_checkout(
    wt: &std::path::Path,
    plan: &grit_lib::revwalk::history::RewritePlan,
) {
    assert_eq!(
        git_rev_parse(wt, "refs/heads/main"),
        Some(plan.ref_updates[0].new),
        "a rewrite whose refs already moved must stay applied"
    );
    assert_eq!(fs::read_to_string(wt.join("one.txt")).unwrap(), "one new\n");
    assert_eq!(
        fs::read_to_string(wt.join("two.txt")).unwrap(),
        "two new, a much longer line\n"
    );
    assert_eq!(fs::read_to_string(wt.join("added.txt")).unwrap(), "added\n");
    assert_eq!(
        fs::read_to_string(wt.join("local.txt")).unwrap(),
        "local edit, longer\n",
        "unrelated local edits must survive recovery"
    );
    let porcelain = git_porcelain(wt);
    let mut lines: Vec<&str> = porcelain.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, [" M local.txt", "?? scratch.txt"], "{porcelain}");
    assert!(!wt.join(".git/grit/rewrite-apply").exists());
    assert!(!wt.join(".git/index.lock").exists());
    git_fsck_strict(wt);
}

#[test]
fn interrupted_rewrite_apply_finishes_half_written_checkout() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    let recovery = open_grit(wt)
        .history()
        .recover_interrupted_apply()
        .expect("recover")
        .expect("recovery outcome");
    assert_eq!(
        recovery,
        grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout
    );
    assert_finished_partial_checkout(wt, &plan);
}

#[test]
fn status_finishes_interrupted_rewrite_apply_and_reports_it() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let model = grit
        .status()
        .run(
            &grit_lib::porcelain::status::StatusOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("status must not fail on an interrupted rewrite");
    assert_eq!(
        model.interrupted_rewrite,
        Some(grit_lib::porcelain::status::InterruptedRewrite::FinishedCheckout)
    );
    assert_finished_partial_checkout(wt, &plan);
    let again = grit
        .status()
        .run(
            &grit_lib::porcelain::status::StatusOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("second status");
    assert_eq!(again.interrupted_rewrite, None);
}

#[test]
fn interrupted_rewrite_apply_recovers_after_simulated_kill9_with_default_pid_sidecar() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let index_path = wt.join(".git/index");
    let lock = grit_lib::index::IndexLock::acquire(&index_path, None)
        .expect("simulate grit holding index.lock mid-checkout");
    std::mem::forget(lock);
    assert!(wt.join(".git/index.lock").is_file());
    assert!(wt.join(".git/index~pid.lock").is_file());
    // kill -9 leaves the lock and sidecar but the holder is gone.
    fs::write(wt.join(".git/index~pid.lock"), "pid 999999999\n").expect("dead holder");

    let recovery = grit
        .history()
        .recover_interrupted_apply()
        .expect("recover")
        .expect("recovery outcome");
    assert_eq!(
        recovery,
        grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout
    );
    assert_finished_partial_checkout(wt, &plan);
}

#[test]
fn interrupted_rewrite_apply_with_lockfile_pid_disabled_leaves_foreign_lock() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    git(wt, &["config", "core.lockfilePid", "false"]);
    fs::write(wt.join(".git/index.lock"), "").expect("lock without sidecar");
    let grit = open_grit(wt);
    let err = grit
        .history()
        .recover_interrupted_apply()
        .expect_err("foreign lock");
    assert!(matches!(
        &err,
        grit_lib::revwalk::history::HistoryError::Engine(e)
            if matches!(
                e.as_ref(),
                grit_lib::revwalk::history::HistoryEngineError::LockHeld(_)
            )
    ));
    assert_eq!(
        git_rev_parse(wt, "refs/heads/main"),
        Some(plan.ref_updates[0].new)
    );
    assert!(wt.join(".git/index.lock").exists());
}

#[test]
fn interrupted_rewrite_apply_removes_stale_index_lock_of_dead_holder() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    fs::write(wt.join(".git/index.lock"), "").expect("stale lock");
    let dead_pid = u64::from(u32::MAX);
    fs::write(wt.join(".git/index~pid.lock"), format!("pid {dead_pid}\n")).expect("sidecar");
    let recovery = open_grit(wt)
        .history()
        .recover_interrupted_apply()
        .expect("recover")
        .expect("recovery outcome");
    assert_eq!(
        recovery,
        grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout
    );
    assert!(!wt.join(".git/index~pid.lock").exists());
    assert_finished_partial_checkout(wt, &plan);
}

#[test]
fn interrupted_rewrite_apply_waits_for_index_lock_holder() {
    let (repo, plan) = interrupted_partial_checkout();
    let wt = repo.worktree();
    let lock = wt.join(".git/index.lock");
    fs::write(&lock, "").expect("held lock");
    let sidecar = wt.join(".git/index~pid.lock");
    fs::write(&sidecar, format!("pid {}\n", std::process::id())).expect("sidecar");
    let grit = open_grit(wt);

    let err = grit
        .history()
        .recover_interrupted_apply()
        .expect_err("lock is held");
    assert!(
        matches!(
            &err,
            grit_lib::revwalk::history::HistoryError::Engine(e)
                if matches!(
                    e.as_ref(),
                    grit_lib::revwalk::history::HistoryEngineError::LockHeld(
                        grit_lib::error::LockHeldError { path, holder: Some(h), .. }
                    ) if *path == lock && h.running
                )
        ),
        "{err:?}"
    );
    assert_eq!(
        git_rev_parse(wt, "refs/heads/main"),
        Some(plan.ref_updates[0].new)
    );
    assert!(grit.history().apply_journal_active());
    assert!(
        lock.exists() && sidecar.exists(),
        "a live holder's lock is never removed"
    );

    let model = grit
        .status()
        .run(
            &grit_lib::porcelain::status::StatusOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("status reads while another process holds the index lock");
    assert_eq!(
        model.interrupted_rewrite,
        Some(grit_lib::porcelain::status::InterruptedRewrite::Pending)
    );
    assert_eq!(
        git_rev_parse(wt, "refs/heads/main"),
        Some(plan.ref_updates[0].new)
    );

    fs::remove_file(&lock).expect("release");
    fs::remove_file(&sidecar).expect("release sidecar");
    assert_eq!(
        grit.history().recover_interrupted_apply().expect("recover"),
        Some(grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout)
    );
    assert_finished_partial_checkout(wt, &plan);
}

#[test]
fn interrupted_rewrite_apply_abort_keeps_racily_clean_edit_visible() {
    use filetime::{set_file_mtime, FileTime};

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    git(wt, &["config", "core.trustctime", "false"]);
    let second = 1_700_000_000;
    fs::write(wt.join("local.txt"), "local base\n").expect("write");
    set_file_mtime(wt.join("local.txt"), FileTime::from_unix_time(second, 100)).expect("mtime");
    git(wt, &["add", "local.txt"]);
    git(wt, &["commit", "-m", "local"]);
    let tip = git_rev_parse(wt, "HEAD").expect("tip");
    // Same size and second as the index entry: only the index's own mtime (also in that second)
    // tells a reader to compare content.
    fs::write(wt.join("local.txt"), "local edit\n").expect("edit");
    set_file_mtime(wt.join("local.txt"), FileTime::from_unix_time(second, 300)).expect("mtime");
    set_file_mtime(wt.join(".git/index"), FileTime::from_unix_time(second, 500)).expect("mtime");

    let plan = plan_tip_tree_edits(wt, tip, &[("other.txt", b"other\n")]);
    let grit = open_grit(wt);
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    fs::write(wt.join("other.txt"), "oth").expect("half-written new file");

    assert_eq!(
        grit.history().abort_interrupted_apply().expect("abort"),
        grit_lib::revwalk::history::RewriteApplyRecovery::RolledBack
    );
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), Some(tip));
    assert!(!wt.join("other.txt").exists());
    assert_eq!(git_porcelain(wt), " M local.txt\n");
}

#[test]
fn rewrite_apply_journal_without_meta_is_removed() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let tip = git_rev_parse(wt, "refs/heads/main");
    let dir = wt.join(".git/grit/rewrite-apply");
    fs::create_dir_all(dir.join("worktree")).expect("orphan dir");
    fs::write(dir.join("index-before"), b"partial").expect("orphan index snapshot");
    fs::write(dir.join("worktree/0123"), b"partial").expect("orphan worktree snapshot");
    let grit = open_grit(wt);

    fs::write(wt.join(".git/index.lock"), "").expect("lock of unknown holder");
    assert_eq!(
        grit.history().recover_interrupted_apply().expect("recover"),
        None
    );
    assert!(
        dir.exists(),
        "a journal being written under index.lock is left alone"
    );

    fs::remove_file(wt.join(".git/index.lock")).expect("unlock");
    assert_eq!(
        grit.history().recover_interrupted_apply().expect("recover"),
        None
    );
    assert!(!dir.exists());
    assert_eq!(git_rev_parse(wt, "refs/heads/main"), tip);
}

#[test]
fn apply_rewrite_journal_accepts_git_path_with_double_dot_in_name() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    commit_file(wt, "a..b", "", "track a..b");
    let grit = open_grit(wt);
    let plan = plan_reword_with_tree(wt, oids["b"], "side.txt", b"side\n", "side change");
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply with a..b present");
    assert!(wt.join("a..b").is_file());
    assert!(wt.join("side.txt").is_file());
}

#[test]
fn rewrite_apply_journal_stores_from_tree_manifest_for_wide_clean_transition() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    for i in 0..40 {
        commit_file(
            wt,
            &format!("wide/{i}.txt"),
            &format!("base {i}\n"),
            &format!("track {i}"),
        );
    }
    let tip = git_rev_parse(wt, "HEAD").expect("tip");
    let edits: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("wide/{i}.txt"), format!("next {i}\n").into_bytes()))
        .collect();
    let edit_refs: Vec<(&str, &[u8])> = edits
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let plan = plan_tip_tree_edits(wt, tip, &edit_refs);
    let grit = open_grit(wt);
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    let worktree_dir = wt.join(".git/grit/rewrite-apply/worktree");
    let file_count = fs::read_dir(&worktree_dir)
        .expect("journal worktree dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .count();
    assert_eq!(
        file_count, 1,
        "wide clean tree transitions must not write one journal payload file per path"
    );
    grit.history().abort_interrupted_apply().expect("abort");
    for i in 0..40 {
        let path = wt.join(format!("wide/{i}.txt"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            format!("base {i}\n"),
            "rollback must rebuild clean paths from the old tree"
        );
    }
    git_fsck_strict(wt);
}

#[test]
fn interrupted_rewrite_apply_abort_restores_empty_tracked_file() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    commit_file(wt, "empty.txt", "", "empty tracked");
    let grit = open_grit(wt);
    let plan = plan_reword_with_tree(
        wt,
        oids["c"],
        "empty.txt",
        b"would-be-new-content\n",
        "change empty in replayed tip",
    );
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    fs::remove_file(wt.join("empty.txt")).expect("simulate partial checkout deleting empty file");
    grit.history().abort_interrupted_apply().expect("abort");
    assert!(wt.join("empty.txt").is_file());
    assert_eq!(fs::read(wt.join("empty.txt")).expect("read"), b"");
}

#[cfg(unix)]
#[test]
fn interrupted_rewrite_apply_abort_restores_newline_filename() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let newline_path = "line\nbreak";
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let tip = commit_file(wt, newline_path, "before\n", "track newline filename");
    let plan = plan_reword_tree_from_commit(
        wt,
        tip,
        newline_path,
        b"after\n",
        "change newline-named file in replayed tip",
    );
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    grit.history()
        .apply(
            &plan,
            &apply_opts(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    fs::remove_file(wt.join(newline_path)).expect("simulate partial checkout");
    grit.history().abort_interrupted_apply().expect("abort");
    assert!(wt.join(newline_path).is_file());
    assert_eq!(fs::read(wt.join(newline_path)).expect("read"), b"before\n");
}

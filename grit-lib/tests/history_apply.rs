//! Integration tests for [`grit_lib::revwalk::history::apply_rewrite`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::process::Command;

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
use grit_lib::refs::reflog::read_reflog;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, plan_rewrite, recover_interrupted_rewrite_apply, rewrite_apply_journal_active,
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
        identity: "Applier <apply@grit.test>".into(),
        reflog_message: "history rewrite".into(),
        record_operation: None,
        hooks: grit_lib::PostOperationHooks::Run,
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
    plan_rewrite(
        &grit,
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
        plan_rewrite(
            &grit,
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
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("head moved");
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
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("stale plan");
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
            committer: committer(2).parse().expect("signature"),
            preserve_committer: false,
            sign: true,
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
    let tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main");
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
    let head_log = read_reflog(grit.git_dir(), "HEAD").expect("parse HEAD reflog");
    let branch_log = read_reflog(grit.git_dir(), "refs/heads/main").expect("parse branch reflog");
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
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let new_tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main tip");
    let head_log = read_reflog(grit.git_dir(), "HEAD").expect("HEAD reflog");
    let branch_log = read_reflog(grit.git_dir(), "refs/heads/main").expect("main reflog");
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
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main");
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
    apply_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let head_log = read_reflog(grit.git_dir(), "HEAD").expect("head log");
    let branch_log = read_reflog(grit.git_dir(), "refs/heads/main").expect("branch log");
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

#[test]
fn apply_rewrite_failed_checkout_restores_refs_and_preserves_dirty_worktree() {
    use grit_lib::repo::set_test_inject_index_write_fail_remaining;

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
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("injected checkout failure");
    grit_lib::repo::set_test_inject_index_write_fail(false);
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
    grit_lib::repo::set_test_inject_rewrite_apply_abort_after_refs(true);
    let err = apply_rewrite(&grit, &plan, &apply_opts()).expect_err("injected abort after refs");
    grit_lib::repo::set_test_inject_rewrite_apply_abort_after_refs(false);
    let grit_lib::revwalk::history::HistoryError::WorktreeApply { message } = err else {
        panic!("unexpected apply error");
    };
    assert!(
        message.contains("injected abort after rewrite ref updates"),
        "unexpected message: {message}"
    );
    assert!(rewrite_apply_journal_active(grit.git_dir()));
    assert_ne!(git_rev_parse(wt, "refs/heads/main"), Some(tip_before));
    assert!(wt.join("rewritten-only.txt").exists() == false);

    let recovery = recover_interrupted_rewrite_apply(&grit)
        .expect("recover")
        .expect("recovery outcome");
    assert_eq!(
        recovery,
        grit_lib::revwalk::history::RewriteApplyRecovery::FinishedCheckout
    );
    assert!(!rewrite_apply_journal_active(grit.git_dir()));
    assert!(wt.join("rewritten-only.txt").exists());
}

//! Tests for [`grit_lib::revwalk::history::reword`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::objects::{parse_commit, parse_tree, ObjectId};
use grit_lib::progress::CancelToken;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    normalize_rewrite_message, ApplyOptions, EmptyPolicy, HistoryError, RefScope, RewordOptions,
    RewriteOptions,
};

use support::{
    build_commit_dag, commit_labeled_with_dates, git, git_fsck_strict, git_rev_parse, HistoryRepo,
};

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
    let plan = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_message("second reworded\n"),
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
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
fn reword_rejects_unwritable_committer_before_writing_objects() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let objects_before = git(wt, &["count-objects", "-v"]);
    // `Signature::new` rejects the injected name, so build a valid one and corrupt it.
    let mut injected = grit_lib::ident::Signature::new(
        "Injected",
        "r@grit.test",
        time::OffsetDateTime::from_unix_timestamp(1).expect("time"),
    )
    .expect("valid signature");
    injected.name = "Injected\nextra-header value".into();
    let err = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_message("second reworded\n"),
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: injected,
                preserve_committer: false,
                sign: true,
            },
        )
        .expect_err("unwritable committer");
    assert!(
        matches!(
            &err,
            HistoryError::Engine(e)
                if matches!(
                    e.as_ref(),
                    grit_lib::revwalk::history::HistoryEngineError::InvalidSignature(_)
                )
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(git(wt, &["count-objects", "-v"]), objects_before);
    assert_eq!(git_rev_parse(wt, "main"), Some(oids["c"]));
    git(wt, &["fsck", "--strict", "--unreachable"]);
}

fn rewrite_opts(seq: i64) -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(seq).parse().expect("signature"),
        preserve_committer: false,
        sign: true,
    }
}

fn git_log_author_fields(wt: &std::path::Path, rev: &str) -> String {
    git(wt, &["log", "-1", rev, "--format=%an|%ae|%ad"])
        .trim()
        .to_owned()
}

#[test]
fn reword_author_only_keeps_message_and_date() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let a = commit_labeled_with_dates(wt, "a", &[], "1000 +0000", "1001 +0000");
    let b = commit_labeled_with_dates(wt, "b", &[a], "5000 +0000", "5001 +0000");
    git(wt, &["update-ref", "refs/heads/main", &b.to_hex()]);
    let before = git_log_author_fields(wt, &b.to_hex());
    let old_msg = parse_commit(&open_grit(wt).odb().read(&b).expect("read b").data)
        .expect("parse b")
        .message;

    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            b,
            &RewordOptions::default().with_author("New Author <new@example.com>"),
            &rewrite_opts(2),
        )
        .expect("reword author");
    grit.history()
        .apply(
            &plan,
            &ApplyOptions {
                identity: support::reflog_signature("Rewriter <r@grit.test>"),
                reflog_message: "history reword".into(),
                record_operation: None,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let new_b = plan
        .commits
        .iter()
        .find(|r| r.old == b)
        .and_then(|r| r.new)
        .expect("new b");
    let new_parsed =
        parse_commit(&grit.odb().read(&new_b).expect("read new").data).expect("parse new");
    assert_eq!(new_parsed.message, old_msg);
    let after = git_log_author_fields(wt, &new_b.to_hex());
    let after_parts: Vec<_> = after.split('|').collect();
    assert_eq!(after_parts[0], "New Author");
    assert_eq!(after_parts[1], "new@example.com");
    assert_eq!(
        after_parts[2],
        before.split('|').nth(2).expect("author date")
    );
    git_fsck_strict(wt);
}

#[test]
fn reword_author_and_message() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default()
                .with_message("both changed\n")
                .with_author("Both Author <both@example.com>"),
            &rewrite_opts(3),
        )
        .expect("reword");
    grit.history()
        .apply(
            &plan,
            &ApplyOptions {
                identity: support::reflog_signature("Rewriter <r@grit.test>"),
                reflog_message: "history reword".into(),
                record_operation: None,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let new_b = plan.replacement(oids["b"]).expect("mapped");
    let fields: Vec<_> = git_log_author_fields(wt, &new_b.to_hex())
        .split('|')
        .map(str::to_owned)
        .collect();
    assert_eq!(fields[0], "Both Author");
    assert_eq!(fields[1], "both@example.com");
    let parsed = parse_commit(&grit.odb().read(&new_b).expect("read").data).expect("parse");
    assert_eq!(parsed.message, "both changed\n");
    git_fsck_strict(wt);
}

#[test]
fn reword_message_with_whitespace_only_author_succeeds() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default()
                .with_message("message only despite empty author opt\n")
                .with_author("   "),
            &rewrite_opts(6),
        )
        .expect("reword message with ignored empty author");
    let new_b = plan.replacement(oids["b"]).expect("mapped");
    let parsed = parse_commit(&grit.odb().read(&new_b).expect("read").data).expect("parse");
    assert_eq!(parsed.message, "message only despite empty author opt\n");
    git_fsck_strict(wt);
}

#[test]
fn reword_requires_message_or_author() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = grit
        .history()
        .reword(oids["b"], &RewordOptions::default(), &rewrite_opts(4))
        .unwrap_err();
    assert!(matches!(err, HistoryError::NothingToReword));
}

#[test]
fn reword_author_rejects_newline() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_author("Bad\nInjected <x@y.example>"),
            &rewrite_opts(5),
        )
        .unwrap_err();
    assert!(matches!(err, HistoryError::InvalidAuthor(_)));
}

#[test]
fn reword_rejects_empty_message() {
    let err = normalize_rewrite_message("   \n\n  ").unwrap_err();
    assert!(matches!(err, HistoryError::EmptyMessage));
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_message("  \t\n"),
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer(1).parse().expect("signature"),
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
    let commit_bytes = grit_lib::objects::serialize_commit(&grit_lib::objects::CommitData::new(
        empty,
        vec![],
        committer(0),
        committer(0),
        "root\n",
    ));
    let oid = grit
        .odb()
        .write(grit_lib::objects::ObjectKind::Commit, &commit_bytes)
        .expect("commit");
    grit_test_support::refs::set_ref(&grit, "refs/heads/main", &oid).expect("ref");
    let plan = grit
        .history()
        .reword(
            oid,
            &RewordOptions::default().with_message("bare reword\n"),
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer(2).parse().expect("signature"),
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
    let plan = grit
        .history()
        .reword(
            root,
            &RewordOptions::default().with_message("rewritten root\n"),
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: committer(10).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("reword root");
    grit.history()
        .apply(
            &plan,
            &ApplyOptions {
                identity: support::reflog_signature("Rewriter <rewriter@test>"),
                reflog_message: "history reword".into(),
                record_operation: None,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");

    let head = grit.references().head().expect("head");
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

fn apply_reword(grit: &Repository, plan: &grit_lib::revwalk::history::RewritePlan) {
    grit.history()
        .apply(
            plan,
            &ApplyOptions {
                identity: support::reflog_signature("Rewriter <r@grit.test>"),
                reflog_message: "history reword".into(),
                record_operation: None,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
}

#[test]
fn reword_head_leaves_backup_branch_at_old_tip() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b")],
        &[("c", "refs/heads/main"), ("c", "refs/heads/backup")],
    );
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            oids["c"],
            &RewordOptions::default().with_message("c reworded\n"),
            &rewrite_opts(1),
        )
        .expect("reword");
    let moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(moved, ["refs/heads/main"]);
    apply_reword(&grit, &plan);
    assert_eq!(git_rev_parse(wt, "backup"), Some(oids["c"]));
    assert_ne!(git_rev_parse(wt, "main"), Some(oids["c"]));
    git_fsck_strict(wt);
}

#[test]
fn reword_keeps_moving_stacked_branches_but_not_same_tip_backup() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[
            ("c", "refs/heads/main"),
            ("c", "refs/heads/backup"),
            ("d", "refs/heads/stacked"),
        ],
    );
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_message("b reworded\n"),
            &rewrite_opts(1),
        )
        .expect("reword");
    let mut moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    moved.sort_unstable();
    assert_eq!(moved, ["refs/heads/main", "refs/heads/stacked"]);
    apply_reword(&grit, &plan);
    assert_eq!(git_rev_parse(wt, "backup"), Some(oids["c"]));
    assert_eq!(
        git_rev_parse(wt, "stacked~1"),
        git_rev_parse(wt, "main"),
        "stacked branch stays on top of the rewritten main"
    );
    git_fsck_strict(wt);
}

#[test]
fn reword_on_detached_head_leaves_branch_at_head_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    git(wt, &["switch", "-q", "--detach", &oids["c"].to_hex()]);
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .reword(
            oids["b"],
            &RewordOptions::default().with_message("b reworded\n"),
            &rewrite_opts(1),
        )
        .expect("reword");
    let moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(moved, ["HEAD"]);
    apply_reword(&grit, &plan);
    assert_eq!(git_rev_parse(wt, "main"), Some(oids["c"]));
    assert_ne!(git_rev_parse(wt, "HEAD"), Some(oids["c"]));
    git_fsck_strict(wt);
}

//! Tests for [`grit_lib::revwalk::history::move_commit`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::parse_commit;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, move_commit, ApplyOptions, EmptyPolicy, HistoryError, Placement, RefScope,
    RewriteOptions,
};

use support::{build_commit_dag, commit_file, git, git_rev_parse_verify, HistoryRepo};

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

fn apply_plan(wt: &std::path::Path, plan: &grit_lib::revwalk::history::RewritePlan) {
    let grit = open_grit(wt);
    apply_rewrite(
        &grit,
        plan,
        &ApplyOptions {
            identity: "T <t@grit.test>".into(),
            reflog_message: "history move".into(),
        },
    )
    .expect("apply");
}

const PINNED_NAME: &str = "History Parity";
const PINNED_EMAIL: &str = "parity@grit.test";
const PINNED_COMMITTER: &str = "History Parity <parity@grit.test> 1700000000 +0000";

fn commit_tree_with_pinned_idents(
    wt: &Path,
    path: &str,
    content: &str,
    message: &str,
    parents: &[grit_lib::objects::ObjectId],
) -> grit_lib::objects::ObjectId {
    let full = wt.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&full, content).expect("write");
    git(wt, &["add", path]);
    let tree = git(wt, &["write-tree"]).trim().to_string();
    let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), message.into()];
    for parent in parents {
        args.push("-p".into());
        args.push(parent.to_hex());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = Command::new("git")
        .current_dir(wt)
        .args(&arg_refs)
        .env("GIT_AUTHOR_NAME", PINNED_NAME)
        .env("GIT_AUTHOR_EMAIL", PINNED_EMAIL)
        .env("GIT_COMMITTER_NAME", PINNED_NAME)
        .env("GIT_COMMITTER_EMAIL", PINNED_EMAIL)
        .env("GIT_AUTHOR_DATE", "@1700000000")
        .env("GIT_COMMITTER_DATE", "@1700000000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("commit-tree");
    assert!(
        out.status.success(),
        "commit-tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("commit oid")
}

fn init_linear_four_pinned(wt: &Path) -> [grit_lib::objects::ObjectId; 4] {
    let a = commit_tree_with_pinned_idents(wt, "step-a.txt", "a\n", "history harness a", &[]);
    git(wt, &["update-ref", "refs/heads/main", &a.to_hex()]);
    let b = commit_tree_with_pinned_idents(wt, "step-b.txt", "b\n", "history harness b", &[a]);
    git(wt, &["update-ref", "refs/heads/main", &b.to_hex()]);
    let c = commit_tree_with_pinned_idents(wt, "step-c.txt", "c\n", "history harness c", &[b]);
    git(wt, &["update-ref", "refs/heads/main", &c.to_hex()]);
    let d = commit_tree_with_pinned_idents(wt, "step-d.txt", "d\n", "history harness d", &[c]);
    git(wt, &["update-ref", "refs/heads/main", &d.to_hex()]);
    [a, b, c, d]
}

fn init_linear_four(wt: &std::path::Path) -> [grit_lib::objects::ObjectId; 4] {
    commit_file(wt, "step-a.txt", "a\n", "history harness a");
    let a = git_rev_parse_verify(wt, "HEAD").expect("a");
    commit_file(wt, "step-b.txt", "b\n", "history harness b");
    let b = git_rev_parse_verify(wt, "HEAD").expect("b");
    commit_file(wt, "step-c.txt", "c\n", "history harness c");
    let c = git_rev_parse_verify(wt, "HEAD").expect("c");
    commit_file(wt, "step-d.txt", "d\n", "history harness d");
    let d = git_rev_parse_verify(wt, "HEAD").expect("d");
    [a, b, c, d]
}

fn subjects(wt: &std::path::Path) -> Vec<String> {
    git(wt, &["log", "--first-parent", "--reverse", "--format=%s"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn move_commit_before_and_after() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let [a, b, c, d] = init_linear_four(wt);
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };

    let plan = move_commit(&grit, c, Placement::Before(b), &opts).expect("move c before b");
    apply_plan(wt, &plan);
    assert_eq!(
        subjects(wt),
        vec![
            "history harness a",
            "history harness c",
            "history harness b",
            "history harness d",
        ]
    );

    let grit = open_grit(wt);
    let b_live = git_rev_parse_verify(wt, "HEAD~1").expect("b");
    let d_tip = git_rev_parse_verify(wt, "HEAD").expect("d");
    let plan = move_commit(&grit, b_live, Placement::After(d_tip), &opts).expect("move b after d");
    apply_plan(wt, &plan);
    assert_eq!(
        subjects(wt).last().map(String::as_str),
        Some("history harness b")
    );
    git_fsck_strict(wt);
    let _ = a;
}

#[test]
fn move_commit_to_top_and_bottom_of_segment() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let [a, b, _c, _d] = init_linear_four(wt);
    commit_file(wt, "step-e.txt", "e\n", "history harness e");
    let tip = git_rev_parse_verify(wt, "HEAD").expect("e");
    let _ = _d;
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };

    let plan = move_commit(&grit, b, Placement::After(tip), &opts).expect("move b to top");
    apply_plan(wt, &plan);
    assert_eq!(
        subjects(wt).last().map(String::as_str),
        Some("history harness b")
    );

    let grit = open_grit(wt);
    let e_oid = git_rev_parse_verify(wt, "HEAD~1").expect("e before moving tip");
    let plan = move_commit(&grit, e_oid, Placement::After(a), &opts).expect("move e after root");
    apply_plan(wt, &plan);
    assert_eq!(subjects(wt)[1], "history harness e");
    git_fsck_strict(wt);
}

#[test]
fn move_noop_errors() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let [_a, b, c, _d] = init_linear_four(wt);
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let err = move_commit(&grit, b, Placement::Before(c), &opts).unwrap_err();
    assert!(matches!(err, HistoryError::NoOp { .. }));
}

#[test]
fn move_with_ancient_merge_on_first_parent_chain() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("side", "a"),
            ("merge", "a"),
            ("merge", "side"),
            ("b", "merge"),
            ("c", "b"),
            ("d", "c"),
        ],
        &[("d", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let plan = move_commit(&grit, oids["c"], Placement::Before(oids["b"]), &opts)
        .expect("move c before b with ancient merge below");
    apply_plan(wt, &plan);
    assert_eq!(
        subjects(wt),
        vec![
            "history harness a",
            "history harness merge",
            "history harness c",
            "history harness b",
            "history harness d",
        ]
    );
    git_fsck_strict(wt);
}

#[test]
fn move_with_forked_topic_branch_succeeds() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c"), ("t", "b")],
        &[("d", "refs/heads/main"), ("t", "refs/heads/topic")],
    );
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let plan = move_commit(&grit, oids["c"], Placement::Before(oids["b"]), &opts)
        .expect("move c before b with forked topic branch");
    apply_plan(wt, &plan);
    assert_eq!(
        subjects(wt),
        vec![
            "history harness a",
            "history harness c",
            "history harness b",
            "history harness d",
        ]
    );
    let topic = git_rev_parse_verify(wt, "refs/heads/topic").expect("topic");
    assert_eq!(
        topic, oids["t"],
        "forked topic branch should stay on its side commit"
    );
    git_fsck_strict(wt);
}

#[test]
fn move_across_merge_rejected() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("b", "a"),
            ("c", "b"),
            ("side", "a"),
            ("merge", "c"),
            ("merge", "side"),
        ],
        &[("merge", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let err = move_commit(&grit, oids["b"], Placement::After(oids["merge"]), &opts).unwrap_err();
    assert!(
        matches!(
            err,
            HistoryError::NotLinear | HistoryError::MergeCommitInReplay { .. }
        ),
        "{err:?}"
    );
}

#[test]
fn move_conflict_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "fight.txt", "1\nbase\n3\n", "history harness a");
    let a = git_rev_parse_verify(wt, "HEAD").expect("a");
    commit_file(wt, "fight.txt", "1\nleft\n3\n", "history harness b");
    let b = git_rev_parse_verify(wt, "HEAD").expect("b");
    commit_file(wt, "fight.txt", "1\nright\n3\n", "history harness c");
    let c = git_rev_parse_verify(wt, "HEAD").expect("c");
    let main_before = git_rev_parse_verify(wt, "HEAD").expect("tip");
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let err = move_commit(&grit, c, Placement::Before(b), &opts).unwrap_err();
    assert!(matches!(err, HistoryError::Conflict { .. }));
    assert_eq!(
        git_rev_parse_verify(wt, "HEAD").expect("unchanged"),
        main_before
    );
    let _ = a;
}

#[test]
fn move_maps_branch_in_segment() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let [_a, b, c, _d] = init_linear_four(wt);
    git(
        wt,
        &[
            "update-ref",
            "refs/heads/topic",
            &git_rev_parse_verify(wt, "HEAD~1").expect("c").to_hex(),
        ],
    );
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer(1),
        preserve_committer: false,
    };
    let plan = move_commit(&grit, c, Placement::Before(b), &opts).expect("move");
    apply_plan(wt, &plan);
    let topic = git_rev_parse_verify(wt, "refs/heads/topic").expect("topic");
    let parsed = parse_commit(&grit.odb().read(&topic).expect("read").data).expect("parse");
    assert_eq!(parsed.message, "history harness c\n");
    assert_ne!(topic, c);
    git_fsck_strict(wt);
}

fn copy_repo(src: &std::path::Path, dest: &std::path::Path) {
    std::fs::create_dir_all(dest).expect("dest");
    let status = Command::new("cp")
        .args(["-a", "--"])
        .arg(format!("{}/.", src.display()))
        .arg(dest)
        .status()
        .expect("cp");
    assert!(status.success(), "cp -a failed");
}

#[test]
fn history_move_matches_git_rebase_reorder() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let [a, b, c, _d] = init_linear_four_pinned(wt);
    let wt2 = tempfile::tempdir().expect("copy dest");
    copy_repo(wt, wt2.path());

    git(wt, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt);
    let opts = RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: PINNED_COMMITTER.into(),
        preserve_committer: false,
    };
    let plan = move_commit(&grit, c, Placement::Before(b), &opts).expect("grit move");
    apply_plan(wt, &plan);

    let b_pick = git(wt2.path(), &["rev-parse", "--short", &b.to_hex()])
        .trim()
        .to_string();
    let c_pick = git(wt2.path(), &["rev-parse", "--short", &c.to_hex()])
        .trim()
        .to_string();
    let editor = wt2.path().join("swap-bc.sh");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\n\
             todo=\"$1\"\n\
             sed \"s/^pick {b_pick} /pick TMP /; s/^pick {c_pick} /pick {b_pick} /; s/^pick TMP /pick {c_pick} /\" \"$todo\" > \"$todo.new\" && mv \"$todo.new\" \"$todo\"\n"
        ),
    )
    .expect("editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let status = Command::new("git")
        .current_dir(wt2.path())
        .args(["rebase", "-i", &a.to_hex()])
        .env("GIT_SEQUENCE_EDITOR", editor.to_string_lossy().as_ref())
        .env("GIT_AUTHOR_NAME", PINNED_NAME)
        .env("GIT_AUTHOR_EMAIL", PINNED_EMAIL)
        .env("GIT_COMMITTER_NAME", PINNED_NAME)
        .env("GIT_COMMITTER_EMAIL", PINNED_EMAIL)
        .env("GIT_AUTHOR_DATE", "@1700000000")
        .env("GIT_COMMITTER_DATE", "@1700000000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("rebase");
    assert!(status.success(), "git rebase -i reorder failed");

    let grit_log = git(wt, &["rev-list", "--reverse", "HEAD"]);
    let git_log = git(wt2.path(), &["rev-list", "--reverse", "HEAD"]);
    assert_eq!(
        grit_log, git_log,
        "grit move should match git rebase reorder (full OID sequence)"
    );
    git_fsck_strict(wt);
    git_fsck_strict(wt2.path());
}

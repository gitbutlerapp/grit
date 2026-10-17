//! Tests for [`grit_lib::history::absorb`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::history::{
    absorb, apply_absorb_rewrite, apply_fixup_rewrite, fixup, staged_fixup_paths, AbsorbOptions,
    ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, RefScope, RewriteOptions,
};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{commit_file, git, git_fsck_strict, git_rev_parse, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: "Absorb <a@grit.test> 1 +0000".into(),
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Absorb <a@grit.test>".into(),
        reflog_message: "history absorb".into(),
    }
}

fn fixup_opts() -> FixupOptions {
    FixupOptions {
        message: None,
        empty: EmptyPolicy::Drop,
        rewrite: rewrite_opts(),
    }
}

fn plan_and_apply_absorb(grit: &Repository, wt: &std::path::Path, absorb_opts: &AbsorbOptions) {
    let plan = absorb(grit, absorb_opts).expect("plan");
    apply_absorb_rewrite(grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn absorb_assigns_hunks_to_last_touching_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "line1\nline2\n", "first");
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(wt, "file.txt", "line1\nchanged\n", "second");
    let middle = git_rev_parse(wt, "HEAD").expect("middle");
    commit_file(wt, "other.txt", "x\n", "third");
    fs::write(wt.join("file.txt"), "line1\nchanged\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(base),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert_eq!(plan.assignments.len(), 1);
    assert_eq!(plan.assignments[0].commit, middle);
    assert!(plan.unassigned.is_empty());
    apply_absorb_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries
        .iter()
        .find(|e| e.name == b"file.txt")
        .expect("file");
    let content = grit.odb().read(&file.oid).expect("blob");
    assert_eq!(
        String::from_utf8_lossy(&content.data),
        "line1\nchanged\nfix\n"
    );
}

#[test]
fn absorb_multiple_targets_single_replay() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "root.txt", "r\n", "root");
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(wt, "a.txt", "a\n", "a");
    let c1 = git_rev_parse(wt, "HEAD").expect("c1");
    commit_file(wt, "b.txt", "b\n", "b");
    let c2 = git_rev_parse(wt, "HEAD").expect("c2");
    commit_file(wt, "tail.txt", "t\n", "tail");
    fs::write(wt.join("a.txt"), "a\nfix-a\n").expect("write");
    fs::write(wt.join("b.txt"), "b\nfix-b\n").expect("write");
    git(wt, &["add", "a.txt", "b.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(base),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert_eq!(plan.assignments.len(), 2);
    let targets: std::collections::HashSet<_> = plan.assignments.iter().map(|a| a.commit).collect();
    assert_eq!(targets.len(), 2);
    assert!(targets.contains(&c1));
    assert!(targets.contains(&c2));
    assert!(plan.plan.commits.len() >= 2);
    apply_absorb_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn absorb_leaves_ambiguous_hunk_staged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "l1\nl2\n", "first");
    let c1 = git_rev_parse(wt, "HEAD").expect("c1");
    fs::write(wt.join("file.txt"), "A\nl2\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "change line1"]);
    fs::write(wt.join("file.txt"), "A\nB\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "change line2"]);
    fs::write(wt.join("file.txt"), "A*\nB*\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(c1),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert!(plan.assignments.is_empty());
    assert_eq!(plan.unassigned.len(), 1);
    let st = status(&grit, &StatusOptions::default(), &mut NullProgress).expect("status");
    assert_eq!(st.staged.len(), 1);
}

#[test]
fn absorb_ignores_new_files() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "b\n", "base");
    fs::write(wt.join("new.txt"), "n\n").expect("write");
    git(wt, &["add", "new.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: None,
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert!(plan.assignments.is_empty());
    assert_eq!(plan.unassigned.len(), 1);
    assert_eq!(
        plan.unassigned[0].reason.as_str(),
        grit_lib::history::UnassignedReason::FileAdded.as_str()
    );
}

#[test]
fn absorb_respects_base() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "old\n", "old");
    commit_file(wt, "file.txt", "mid\n", "mid");
    let mid = git_rev_parse(wt, "HEAD").expect("mid");
    commit_file(wt, "other.txt", "x\n", "tail");
    fs::write(wt.join("file.txt"), "mid\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(mid),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert!(plan.assignments.is_empty());
    assert!(!plan.unassigned.is_empty());
}

#[test]
fn absorb_dry_run_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "a\n", "a");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "file.txt", "a\nb\n", "b");
    fs::write(wt.join("file.txt"), "a\nb\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let before = git_rev_parse(wt, "HEAD").expect("before");
    let grit = open_grit(wt);
    let _plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(target),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(before));
}

#[test]
fn absorb_result_equals_manual_fixups() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "root.txt", "r\n", "root");
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(wt, "a.txt", "a\n", "a");
    let c1 = git_rev_parse(wt, "HEAD").expect("c1");
    commit_file(wt, "b.txt", "b\n", "b");
    let c2 = git_rev_parse(wt, "HEAD").expect("c2");
    commit_file(wt, "tail.txt", "t\n", "tail");
    fs::write(wt.join("a.txt"), "a\nfix-a\n").expect("write");
    fs::write(wt.join("b.txt"), "b\nfix-b\n").expect("write");
    git(wt, &["add", "a.txt", "b.txt"]);
    let grit = open_grit(wt);
    plan_and_apply_absorb(
        &grit,
        wt,
        &AbsorbOptions {
            base: Some(base),
            rewrite: rewrite_opts(),
        },
    );
    let absorb_tip = git_rev_parse(wt, "HEAD").expect("absorb tip");

    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "root.txt", "r\n", "root");
    commit_file(wt2, "a.txt", "a\n", "a");
    let c1b = git_rev_parse(wt2, "HEAD").expect("c1");
    commit_file(wt2, "b.txt", "b\n", "b");
    let c2b = git_rev_parse(wt2, "HEAD").expect("c2");
    commit_file(wt2, "tail.txt", "t\n", "tail");
    fs::write(wt2.join("a.txt"), "a\nfix-a\n").expect("write");
    git(wt2, &["add", "a.txt"]);
    let grit2 = open_grit(wt2);
    let staged = staged_fixup_paths(&grit2).expect("staged");
    let plan1 = fixup(&grit2, c1b, &fixup_opts()).expect("fixup1");
    apply_fixup_rewrite(&grit2, &plan1, &apply_opts(), &staged).expect("apply1");
    fs::write(wt2.join("b.txt"), "b\nfix-b\n").expect("write");
    git(wt2, &["add", "b.txt"]);
    let staged2 = staged_fixup_paths(&grit2).expect("staged2");
    let c2_target = plan1
        .commits
        .iter()
        .find(|row| row.old == c2b)
        .and_then(|row| row.new)
        .unwrap_or(c2b);
    let plan2 = fixup(&grit2, c2_target, &fixup_opts()).expect("fixup2");
    apply_fixup_rewrite(&grit2, &plan2, &apply_opts(), &staged2).expect("apply2");
    git_fsck_strict(wt2);
    let manual_tip = git_rev_parse(wt2, "HEAD").expect("manual tip");
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).expect("a"),
        fs::read_to_string(wt2.join("a.txt")).expect("a2")
    );
    assert_eq!(
        fs::read_to_string(wt.join("b.txt")).expect("b"),
        fs::read_to_string(wt2.join("b.txt")).expect("b2")
    );
    fn tip_tree(repo: &Repository, tip: ObjectId) -> ObjectId {
        parse_commit(&repo.odb().read(&tip).expect("read").data)
            .expect("parse")
            .tree
    }
}

#[test]
fn absorb_nothing_staged_errors() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "only.t", "x\n", "only");
    let grit = open_grit(wt);
    let err = absorb(
        &grit,
        &AbsorbOptions {
            base: None,
            rewrite: rewrite_opts(),
        },
    )
    .unwrap_err();
    assert_eq!(err, HistoryError::NothingStaged);
}

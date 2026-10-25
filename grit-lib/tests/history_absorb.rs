//! Tests for [`grit_lib::revwalk::history::absorb`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    absorb, apply_absorb_rewrite, apply_fixup_rewrite, fixup, staged_fixup_paths, AbsorbOptions,
    ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, RefScope, RewriteOptions,
    UnassignedReason,
};

use support::{commit_file, git, git_fsck_strict, git_rev_parse, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: "Absorb <a@grit.test> 1 +0000".parse().expect("signature"),
        preserve_committer: false,
        sign: true,
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: support::reflog_signature("Absorb <a@grit.test>"),
        reflog_message: "history absorb".into(),
        record_operation: None,
        hooks: grit_lib::PostOperationHooks::Run,
    }
}

fn fixup_opts() -> FixupOptions {
    FixupOptions {
        selection: None,
        pathspecs: Vec::new(),
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
    let st = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
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
        UnassignedReason::FileAdded.as_str()
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
fn absorb_dry_run_leaves_index_file_unchanged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "a\n", "a");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "file.txt", "a\nb\n", "b");
    fs::write(wt.join("file.txt"), "a\nb\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let index_path = wt.join(".git/index");
    let before_index = fs::read(&index_path).expect("read index");
    let grit = open_grit(wt);
    let _plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(target),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    let after_index = fs::read(&index_path).expect("read index");
    assert_eq!(
        before_index, after_index,
        "absorb planning must not rewrite index"
    );
}

#[test]
fn absorb_dry_run_assignment_matches_apply() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "line1\nline2\n", "first");
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(wt, "file.txt", "line1\nchanged\n", "second");
    fs::write(wt.join("file.txt"), "line1\nchanged\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let opts = AbsorbOptions {
        base: Some(base),
        rewrite: rewrite_opts(),
    };
    let plan = absorb(&grit, &opts).expect("plan");
    let dry_assignments = plan.assignments.clone();
    assert_eq!(dry_assignments.len(), 1);
    assert_eq!(dry_assignments[0].subject, "second");
    apply_absorb_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    let new_target = plan
        .plan
        .commits
        .iter()
        .find(|row| row.old == dry_assignments[0].commit)
        .and_then(|row| row.new)
        .expect("rewritten target");
    let fixed = parse_commit(&grit.odb().read(&new_target).expect("read").data).expect("parse");
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
    let pre_rewrite = git_rev_parse(wt, "HEAD").expect("pre");

    fn stage_absorb_hunks(wt: &std::path::Path) {
        fs::write(wt.join("a.txt"), "a\nfix-a\n").expect("write");
        fs::write(wt.join("b.txt"), "b\nfix-b\n").expect("write");
        git(wt, &["add", "a.txt", "b.txt"]);
    }

    fs::write(wt.join("a.txt"), "a\nfix-a\n").expect("write");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    let staged = staged_fixup_paths(&grit).expect("staged");
    let plan1 = fixup(&grit, c1, &fixup_opts()).expect("fixup1");
    apply_fixup_rewrite(&grit, &plan1, &apply_opts(), &staged).expect("apply1");
    fs::write(wt.join("b.txt"), "b\nfix-b\n").expect("write");
    git(wt, &["add", "b.txt"]);
    let grit = open_grit(wt);
    let staged2 = staged_fixup_paths(&grit).expect("staged2");
    let c2_target = plan1
        .commits
        .iter()
        .find(|row| row.old == c2)
        .and_then(|row| row.new)
        .unwrap_or(c2);
    let plan2 = fixup(&grit, c2_target, &fixup_opts()).expect("fixup2");
    apply_fixup_rewrite(&grit, &plan2, &apply_opts(), &staged2).expect("apply2");
    git_fsck_strict(wt);
    let manual_tip = git_rev_parse(wt, "HEAD").expect("manual tip");
    let manual_c1_new = plan1
        .commits
        .iter()
        .find(|row| row.old == c1)
        .and_then(|row| row.new)
        .expect("manual c1");
    let manual_c2_new = plan2
        .commits
        .iter()
        .find(|row| row.old == c2_target)
        .and_then(|row| row.new)
        .expect("manual c2");

    git(wt, &["reset", "--hard", &pre_rewrite.to_hex()]);
    stage_absorb_hunks(wt);
    let grit = open_grit(wt);
    let absorb_opts = AbsorbOptions {
        base: Some(base),
        rewrite: rewrite_opts(),
    };
    let absorb_plan = absorb(&grit, &absorb_opts).expect("plan");
    apply_absorb_rewrite(&grit, &absorb_plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);
    let absorb_tip = git_rev_parse(wt, "HEAD").expect("absorb tip");
    let c1_new = absorb_plan
        .plan
        .commits
        .iter()
        .find(|row| row.old == c1)
        .and_then(|row| row.new)
        .expect("c1 rewritten");
    let c2_new = absorb_plan
        .plan
        .commits
        .iter()
        .find(|row| row.old == c2)
        .and_then(|row| row.new)
        .expect("c2 rewritten");

    assert_eq!(c1_new, manual_c1_new, "c1 rewrite OID");
    assert_eq!(c2_new, manual_c2_new, "c2 rewrite OID");
    assert_eq!(
        absorb_tip, manual_tip,
        "tip commit OID must match sequential fixups"
    );
}

#[test]
fn absorb_leaves_unassigned_hunks_on_same_path_staged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let head_lines = |l2: &str, l4: &str, l7: &str| {
        format!("line-1\n{l2}\nline-3\n{l4}\nline-5\nline-6\n{l7}\nline-8\n")
    };
    commit_file(
        wt,
        "file.txt",
        &head_lines("line-2", "line-4", "line-7"),
        "root",
    );
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(
        wt,
        "file.txt",
        &head_lines("line-2", "changed-4", "line-7"),
        "touch line 4",
    );
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "tail.txt", "t\n", "tail");
    let staged = head_lines("unassigned-two", "fixed-4", "unassigned-five");
    fs::write(wt.join("file.txt"), &staged).expect("write");
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
    assert_eq!(plan.assignments[0].commit, target);
    assert_eq!(plan.unassigned.len(), 2);
    apply_absorb_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);

    let head_file = fs::read_to_string(wt.join("file.txt")).expect("worktree");
    assert!(
        head_file.contains("line-2\n") && !head_file.contains("unassigned-two"),
        "committed/worktree HEAD must not include unassigned line-2 edit: {head_file:?}"
    );
    assert!(
        head_file.contains("line-7\n") && !head_file.contains("unassigned-five"),
        "committed/worktree HEAD must not include unassigned line-7 edit: {head_file:?}"
    );
    assert!(head_file.contains("fixed-4"));

    let index_text = git(wt, &["show", ":file.txt"]);
    assert!(
        index_text.contains("unassigned-two"),
        "index must retain unassigned line-2 hunk: {index_text}"
    );
    assert!(
        index_text.contains("unassigned-five"),
        "index must retain unassigned line-7 hunk: {index_text}"
    );
    assert_eq!(
        index_text.matches("unassigned-five").count(),
        1,
        "index must not duplicate unassigned line-7 text: {index_text}"
    );
}

#[test]
fn absorb_coordinate_safe_insertion_with_unassigned_line8() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base8 =
        |l4: &str| format!("line-1\nline-2\nline-3\n{l4}\nline-5\nline-6\nline-7\nline-8\n");
    commit_file(wt, "file.txt", &base8("line-4"), "root");
    let base = git_rev_parse(wt, "HEAD").expect("base");
    commit_file(wt, "file.txt", &base8("changed-4"), "touch line 4");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "tail.txt", "t\n", "tail");
    let staged = format!(
        "line-1\nline-2\nline-3\nassigned-ins\nline-4\nline-5\nline-6\nline-7\nunassigned-8\n"
    );
    fs::write(wt.join("file.txt"), &staged).expect("write");
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
    assert_eq!(plan.assignments.len(), 1, "{plan:?}");
    assert_eq!(plan.unassigned.len(), 1);
    assert_eq!(plan.unassigned[0].reason, UnassignedReason::NoCandidate);
    apply_absorb_rewrite(&grit, &plan, &apply_opts()).expect("apply");
    git_fsck_strict(wt);

    let head_file = fs::read_to_string(wt.join("file.txt")).expect("worktree");
    assert!(
        head_file.contains("line-3\nassigned-ins\n"),
        "insertion must follow line-3 in HEAD: {head_file:?}"
    );
    assert!(
        !head_file.contains("line-2\nassigned-ins"),
        "insertion must not precede line-3: {head_file:?}"
    );
    assert!(
        !head_file.contains("unassigned-8"),
        "HEAD must not contain unassigned line-8 edit: {head_file:?}"
    );
    assert!(head_file.contains("line-8\n") || head_file.ends_with("line-8"));
    let index_text = git(wt, &["show", ":file.txt"]);
    assert_eq!(
        index_text, staged,
        "index must retain full staged shape including unassigned line-8"
    );
}

#[test]
fn absorb_empty_stack_leaves_hunks_unassigned() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "solo.txt", "only\n", "solo");
    let tip = git_rev_parse(wt, "HEAD").expect("tip");
    fs::write(wt.join("solo.txt"), "only\nstaged\n").expect("write");
    git(wt, &["add", "solo.txt"]);
    let grit = open_grit(wt);
    let plan = absorb(
        &grit,
        &AbsorbOptions {
            base: Some(tip),
            rewrite: rewrite_opts(),
        },
    )
    .expect("plan");
    assert!(plan.assignments.is_empty());
    assert_eq!(plan.unassigned.len(), 1);
    assert!(plan.plan.commits.is_empty());
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
    assert!(matches!(err, HistoryError::NothingStaged));
}

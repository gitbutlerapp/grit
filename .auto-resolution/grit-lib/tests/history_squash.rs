//! Tests for [`grit_lib::revwalk::history::squash`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::objects::parse_commit;
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, squash, ApplyOptions, EmptyPolicy, HistoryError, RefScope, RewriteOptions,
    RewritePlan, SquashMessage, SquashOptions,
};

use support::{
    build_commit_dag, commit_file, git, git_fsck_strict, git_rev_parse_verify, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn committer(seq: i64) -> String {
    format!("Squasher <s@grit.test> {seq} +0000")
}

fn squash_opts(scope: RefScope) -> SquashOptions {
    SquashOptions {
        message: SquashMessage::Oldest,
        rewrite: RewriteOptions {
            scope,
            empty: EmptyPolicy::Drop,
            committer: committer(1),
            preserve_committer: false,
            sign: true,
        },
    }
}

fn apply_plan(repo: &Repository, plan: &RewritePlan) {
    apply_rewrite(
        repo,
        plan,
        &ApplyOptions {
            identity: "Squasher <s@grit.test>".into(),
            reflog_message: "history squash".into(),
            record_operation: None,
        },
    )
    .expect("apply");
}

fn range_spec(from: &grit_lib::objects::ObjectId, to: &grit_lib::objects::ObjectId) -> String {
    format!("{}..{}", from.to_hex(), to.to_hex())
}

#[test]
fn squash_folds_linear_range_into_oldest() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let plan = squash(
        &grit,
        &[&range_spec(&oids["a"], &oids["d"])],
        &squash_opts(RefScope::Branches),
    )
    .expect("squash");
    apply_plan(&grit, &plan);
    let main = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main");
    let parents = parse_commit(&grit.odb().read(&main).expect("read").data)
        .expect("parse")
        .parents;
    assert_eq!(parents, vec![oids["a"]]);
    let count_out = git(
        wt,
        &[
            "rev-list",
            "--count",
            &format!("{}..main", oids["a"].to_hex()),
        ],
    );
    assert_eq!(count_out.trim(), "1");
    git_fsck_strict(wt);
}

#[test]
fn squash_interior_range_replays_newer_commits() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let plan = squash(
        &grit,
        &[&range_spec(&oids["a"], &oids["c"])],
        &squash_opts(RefScope::Branches),
    )
    .expect("squash");
    apply_plan(&grit, &plan);
    let subjects: Vec<_> = git(
        wt,
        &[
            "log",
            "--format=%s",
            &format!("{}..main", oids["a"].to_hex()),
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(subjects.len(), 2);
    assert_eq!(subjects[0], "history harness d");
    assert_eq!(subjects[1], "history harness b");
    git_fsck_strict(wt);
}

#[test]
fn squash_keeps_oldest_author_and_tip_tree() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1", "base");
    let a = git_rev_parse_verify(wt, "HEAD").expect("a");
    git(
        wt,
        &[
            "-c",
            "user.name=One",
            "-c",
            "user.email=one@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "one",
        ],
    );
    let b = git_rev_parse_verify(wt, "HEAD").expect("b");
    commit_file(wt, "f", "2", "two");
    let c = git_rev_parse_verify(wt, "HEAD").expect("c");
    git(wt, &["update-ref", "refs/heads/main", &c.to_hex()]);
    let grit = open_grit(wt);
    let plan = squash(
        &grit,
        &[&range_spec(&a, &c)],
        &squash_opts(RefScope::Branches),
    )
    .expect("squash");
    let folded = plan
        .commits
        .iter()
        .find(|r| r.old == b)
        .and_then(|r| r.new)
        .expect("folded b");
    let old_b = parse_commit(&grit.odb().read(&b).expect("read").data).expect("parse");
    let new_b = parse_commit(&grit.odb().read(&folded).expect("read").data).expect("parse");
    let tip_c = parse_commit(&grit.odb().read(&c).expect("read").data).expect("parse");
    assert_eq!(new_b.author, old_b.author);
    assert_eq!(new_b.tree, tip_c.tree);
    git_fsck_strict(wt);
}

#[test]
fn squash_rejects_disconnected_range() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("side", "a")],
        &[("c", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let exclude = format!("^{}", oids["a"].to_hex());
    let err = squash(
        &grit,
        &[
            exclude.as_str(),
            oids["side"].to_hex().as_str(),
            oids["c"].to_hex().as_str(),
        ],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::MultipleTips), "{err:?}");
}

#[test]
fn squash_rejects_root() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&format!("{}..{}", oids["a"].to_hex(), oids["b"].to_hex())],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(
        matches!(err, HistoryError::Planning { .. }) && err.to_string().contains("single commit")
    );
}

#[test]
fn squash_rejects_multiple_tips() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("tip", "a")],
        &[("c", "refs/heads/main")],
    );
    let grit = open_grit(wt);
    let exclude = format!("^{}", oids["a"].to_hex());
    let tip_hex = oids["tip"].to_hex();
    let c_hex = oids["c"].to_hex();
    let err = squash(
        &grit,
        &[exclude.as_str(), tip_hex.as_str(), c_hex.as_str()],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::MultipleTips), "{err:?}");
}

#[test]
fn squash_marker_outside_range_refused() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1", "base");
    let a = git_rev_parse_verify(wt, "HEAD").expect("a");
    commit_file(wt, "f", "2", "one");
    let one = git_rev_parse_verify(wt, "HEAD").expect("one");
    commit_file(wt, "f", "3", "fixup! one");
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&format!("{}..HEAD", one.to_hex())],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        HistoryError::MarkerTargetOutsideRange { .. } | HistoryError::Planning { .. }
    ));
    let _ = (a, one);
}

#[test]
fn squash_amend_marker_replaces_message() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1", "base");
    let a = git_rev_parse_verify(wt, "HEAD").expect("a");
    commit_file(wt, "f", "2", "target");
    let target = git_rev_parse_verify(wt, "HEAD").expect("target");
    git(
        wt,
        &[
            "commit",
            "--allow-empty",
            "-m",
            &format!("amend! target\n\nnew body\n"),
        ],
    );
    commit_file(wt, "f", "3", "tail");
    git(wt, &["update-ref", "refs/heads/main", "HEAD"]);
    let grit = open_grit(wt);
    let plan = squash(
        &grit,
        &[&format!("{}..HEAD~1", a.to_hex())],
        &squash_opts(RefScope::Branches),
    )
    .expect("squash");
    let new_target = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .and_then(|r| r.new)
        .expect("rewritten target");
    let msg = parse_commit(&grit.odb().read(&new_target).expect("read").data)
        .expect("parse")
        .message;
    assert!(msg.contains("new body"));
}

#[test]
fn squash_oldest_merge_preserves_parents() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1", "base");
    commit_file(wt, "f", "2", "mainline");
    let mainline = git_rev_parse_verify(wt, "HEAD").expect("mainline");
    git(wt, &["branch", "side", "HEAD~1"]);
    git(wt, &["checkout", "side"]);
    commit_file(wt, "g", "1", "side work");
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "--no-edit"]);
    let merge = git_rev_parse_verify(wt, "HEAD").expect("merge");
    commit_file(wt, "h", "1", "after merge");
    let tip = git_rev_parse_verify(wt, "HEAD").expect("tip");
    let exclude = format!("^{}", mainline.to_hex());
    let grit = open_grit(wt);
    let plan = squash(
        &grit,
        &[
            exclude.as_str(),
            merge.to_hex().as_str(),
            tip.to_hex().as_str(),
        ],
        &squash_opts(RefScope::Head),
    )
    .expect("squash");
    let folded = plan
        .commits
        .iter()
        .find(|r| r.old == merge)
        .and_then(|r| r.new)
        .expect("folded merge");
    let parents = parse_commit(&grit.odb().read(&folded).expect("read").data)
        .expect("parse")
        .parents;
    assert_eq!(parents.len(), 2);
    git_fsck_strict(wt);
}

#[test]
fn squash_divergent_branch_refused_unless_update_refs_head() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("d", "c")],
        &[("d", "refs/heads/main")],
    );
    write_ref(open_grit(wt).git_dir(), "refs/heads/mid", &oids["b"]).expect("mid ref");
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&range_spec(&oids["a"], &oids["d"])],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(
        matches!(err, HistoryError::DivergentBranch { .. }),
        "{err:?}"
    );
    let side_before = resolve_ref(grit.git_dir(), "refs/heads/mid").expect("mid");
    let plan = squash(
        &grit,
        &[&range_spec(&oids["a"], &oids["d"])],
        &squash_opts(RefScope::Head),
    )
    .expect("head-only");
    apply_plan(&grit, &plan);
    let side_after = resolve_ref(grit.git_dir(), "refs/heads/mid").expect("mid after");
    assert_eq!(side_before, side_after);
    assert!(!plan.ref_updates.iter().any(|u| u.name.contains("mid")));
}

#[test]
fn squash_divergent_side_branch_forked_from_range() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1", "base");
    let base = git_rev_parse_verify(wt, "HEAD").expect("base");
    commit_file(wt, "f", "2", "one");
    let one = git_rev_parse_verify(wt, "HEAD").expect("one");
    git(wt, &["branch", "topic"]);
    git(wt, &["checkout", "topic"]);
    commit_file(wt, "g", "1", "side");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "f", "3", "two");
    let two = git_rev_parse_verify(wt, "HEAD").expect("two");
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&format!("{}..{}", base.to_hex(), two.to_hex())],
        &squash_opts(RefScope::Branches),
    )
    .unwrap_err();
    assert!(
        matches!(err, HistoryError::DivergentBranch { .. }),
        "{err:?}"
    );
}

fn seed_alpha_outside_marker_chain(wt: &std::path::Path) -> grit_lib::objects::ObjectId {
    commit_file(wt, "f", "1", "alpha");
    git_rev_parse_verify(wt, "HEAD").expect("alpha")
}

#[test]
fn squash_refuses_mixed_range_outside_target_with_combine() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let alpha = seed_alpha_outside_marker_chain(wt);
    git(
        wt,
        &["commit", "--allow-empty", "-m", "squash! alpha\n\nextra\n"],
    );
    commit_file(wt, "f", "2", "tail");
    git(wt, &["update-ref", "refs/heads/main", "HEAD"]);
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&format!("{}..HEAD", alpha.to_hex())],
        &SquashOptions {
            message: SquashMessage::Combine,
            rewrite: squash_opts(RefScope::Head).rewrite,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, HistoryError::MarkerTargetOutsideRange { .. }),
        "{err:?}"
    );
}

#[test]
fn squash_refuses_mixed_range_outside_target_with_custom_message() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let alpha = seed_alpha_outside_marker_chain(wt);
    git(
        wt,
        &["commit", "--allow-empty", "-m", "squash! alpha\n\nextra\n"],
    );
    commit_file(wt, "f", "2", "tail");
    git(wt, &["update-ref", "refs/heads/main", "HEAD"]);
    let grit = open_grit(wt);
    let err = squash(
        &grit,
        &[&format!("{}..HEAD", alpha.to_hex())],
        &SquashOptions {
            message: SquashMessage::Custom("replacement\n".into()),
            rewrite: squash_opts(RefScope::Head).rewrite,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, HistoryError::MarkerTargetOutsideRange { .. }),
        "{err:?}"
    );
}

#[test]
fn squash_all_markers_for_one_target_allowed() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let alpha = seed_alpha_outside_marker_chain(wt);
    git(
        wt,
        &["commit", "--allow-empty", "-m", "squash! alpha\n\nextra\n"],
    );
    git(wt, &["commit", "--allow-empty", "-m", "fixup! alpha"]);
    git(wt, &["update-ref", "refs/heads/main", "HEAD"]);
    let grit = open_grit(wt);
    squash(
        &grit,
        &[&format!("{}..HEAD", alpha.to_hex())],
        &SquashOptions {
            message: SquashMessage::Combine,
            rewrite: squash_opts(RefScope::Head).rewrite,
        },
    )
    .expect("all-marker outside target squash");
}

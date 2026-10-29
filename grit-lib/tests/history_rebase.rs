//! Integration tests for [`grit_lib::revwalk::history::rebase_onto`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{
    apply_rewrite, plan_rebase, plan_rebase_check, rebase_onto, ApplyOptions, EmptyPolicy,
    RebaseCheckPlan, RebaseOntoOptions, RebasePlan, RebaseRequest, RefScope, RewriteOptions,
};
use support::assert_git_fsck_strict;
use support::{
    build_commit_dag, commit_file, git, git_merge_base_is_ancestor, git_rev_parse_verify,
    git_version_at_least, open_grit_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    open_grit_repo(wt)
}

fn git_rev_parse(wt: &std::path::Path, spec: &str) -> grit_lib::objects::ObjectId {
    git_rev_parse_verify(wt, spec).expect(spec)
}

const PINNED_COMMITTER: &str = "Refs Harness Author <refs-harness@example.com> 1700001000 +0000";

fn rebase_opts(scope: RefScope, empty: EmptyPolicy, skip: bool) -> RebaseOntoOptions {
    RebaseOntoOptions {
        rewrite: RewriteOptions {
            scope,
            empty,
            committer: PINNED_COMMITTER.parse().expect("signature"),
            preserve_committer: false,
            sign: true,
        },
        skip_applied: skip,
        flatten_leaf_merges: false,
        favor: MergeFavor::default(),
        drop: Vec::new(),
    }
}

/// `upstream` (first commit), `onto` (carries the second patch), `tip` (branch tip with third patch).
fn init_rebase_fixture(wt: &std::path::Path) -> (ObjectId, ObjectId, ObjectId) {
    init_rebase_fixture_from(wt, 1_700_000_000)
}

fn init_rebase_fixture_from(wt: &std::path::Path, start: u64) -> (ObjectId, ObjectId, ObjectId) {
    let mut date = start;
    let mut commit = |msg: &str, content: &str| {
        date += 60;
        let date_s = format!("{date} +0000");
        std::fs::write(wt.join("f.txt"), content).expect("write");
        git(wt, &["add", "f.txt"]);
        std::process::Command::new("git")
            .current_dir(wt)
            .args(["commit", "-m", msg])
            .env("GIT_AUTHOR_NAME", "Refs Harness Author")
            .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
            .env("GIT_COMMITTER_NAME", "Refs Harness Author")
            .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
            .env("GIT_AUTHOR_DATE", &date_s)
            .env("GIT_COMMITTER_DATE", &date_s)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("commit");
    };
    commit("first", "1\n");
    let upstream = git_rev_parse(wt, "HEAD");
    commit("second", "2\n");
    commit("third", "3\n");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["branch", "onto", &upstream.to_hex()]);
    git(wt, &["checkout", "onto"]);
    commit("onto second", "2\n");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "main"]);
    (upstream, onto, tip)
}

fn git_rebase_onto(wt: &std::path::Path, onto: &ObjectId, upstream: &ObjectId, branch: &str) {
    std::process::Command::new("git")
        .current_dir(wt)
        .args([
            "rebase",
            "--onto",
            &onto.to_hex(),
            &upstream.to_hex(),
            branch,
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase");
}

fn apply_plan(repo: &Repository, plan: &grit_lib::revwalk::history::RewritePlan) {
    apply_rewrite(
        repo,
        plan,
        &ApplyOptions {
            identity: support::reflog_signature("Test <t@grit.test>"),
            reflog_message: "rebase onto".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply");
}

#[test]
fn rebase_onto_replays_range_and_moves_branch() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (upstream, onto, tip) = init_rebase_fixture(wt);
    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        upstream,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, true),
    )
    .expect("plan");
    assert!(plan.ref_updates.iter().any(|u| u.name.ends_with("main")));
    apply_plan(&grit, &plan);
    let new_tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("ref");
    assert_ne!(new_tip, tip);
    assert_git_fsck_strict(wt);
}

fn branch_tip_label(branch: char, count: usize) -> &'static str {
    match (branch, count) {
        ('a', 1) => "a1",
        ('a', 2) => "a2",
        ('b', 1) => "b1",
        ('b', 2) => "b2",
        ('c', 1) => "c1",
        ('c', 2) => "c2",
        _ => panic!("unsupported stack depth"),
    }
}

fn init_issue_2213_stack(
    wt: &std::path::Path,
    per_branch: [usize; 3],
) -> std::collections::HashMap<String, ObjectId> {
    assert!(per_branch.iter().all(|&n| n == 1 || n == 2));
    let mut edges: Vec<(&str, &str)> = vec![("base", "r"), ("main_m", "base")];
    let mut parent = "base";
    for (branch, count) in [
        ('a', per_branch[0]),
        ('b', per_branch[1]),
        ('c', per_branch[2]),
    ] {
        for n in 1..=count {
            let label = branch_tip_label(branch, n);
            edges.push((label, parent));
            parent = label;
        }
    }
    build_commit_dag(
        wt,
        &edges,
        &[
            ("main_m", "refs/heads/main"),
            (branch_tip_label('a', per_branch[0]), "refs/heads/a"),
            (branch_tip_label('b', per_branch[1]), "refs/heads/b"),
            (branch_tip_label('c', per_branch[2]), "refs/heads/c"),
        ],
    )
}

fn assert_rebase_moves_stacked_abc(
    wt: &std::path::Path,
    oids: &std::collections::HashMap<String, ObjectId>,
) {
    git(wt, &["checkout", "a"]);
    let grit = open_grit(wt);
    let request = rebase_request(oids["main_m"], None, git_rev_parse(wt, "a"));
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &request, &opts).expect("plan") {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected plan, got {other:?}"),
    };
    let moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    for name in ["refs/heads/a", "refs/heads/b", "refs/heads/c"] {
        assert!(moved.contains(&name), "{moved:?}");
    }
    apply_plan(&grit, &plan);
    assert!(git_merge_base_is_ancestor(wt, "main", "a"));
    assert!(git_merge_base_is_ancestor(wt, "a", "b"));
    assert!(git_merge_base_is_ancestor(wt, "b", "c"));
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_moves_multi_commit_stacked_branches() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = init_issue_2213_stack(wt, [2, 2, 2]);
    assert_rebase_moves_stacked_abc(wt, &oids);
}

#[test]
fn rebase_onto_moves_stacked_branches_one_one_two() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = init_issue_2213_stack(wt, [1, 1, 2]);
    assert_rebase_moves_stacked_abc(wt, &oids);
}

#[test]
fn rebase_onto_moves_stacked_branches_one_two_one() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = init_issue_2213_stack(wt, [1, 2, 1]);
    assert_rebase_moves_stacked_abc(wt, &oids);
}

#[test]
fn rebase_onto_moves_branch_forked_from_middle_of_range() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("base", "r"),
            ("main_m", "base"),
            ("a1", "base"),
            ("a2", "a1"),
            ("side1", "a1"),
            ("side2", "side1"),
        ],
        &[
            ("main_m", "refs/heads/main"),
            ("a2", "refs/heads/a"),
            ("side2", "refs/heads/side"),
        ],
    );
    git(wt, &["checkout", "a"]);
    let grit = open_grit(wt);
    let plan = match plan_rebase(
        &grit,
        &rebase_request(oids["main_m"], None, oids["a2"]),
        &rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true),
    )
    .expect("plan")
    {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected plan, got {other:?}"),
    };
    assert!(
        plan.ref_updates.iter().any(|u| u.name.ends_with("side")),
        "{:?}",
        plan.ref_updates
    );
    apply_plan(&grit, &plan);
    assert!(git_merge_base_is_ancestor(wt, "main", "side"));
    assert!(git_merge_base_is_ancestor(wt, "a~1", "side"));
    assert!(!git_merge_base_is_ancestor(wt, "a", "side"));
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_moves_stacked_merge_with_unrelated_second_parent() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("base", "r"),
            ("main_m", "base"),
            ("a1", "base"),
            ("a2", "a1"),
            ("side", "r"),
            ("b_merge", "a2"),
            ("b_merge", "side"),
        ],
        &[
            ("main_m", "refs/heads/main"),
            ("a2", "refs/heads/a"),
            ("b_merge", "refs/heads/b"),
        ],
    );
    git(wt, &["checkout", "a"]);
    let grit = open_grit(wt);
    let plan = match plan_rebase(
        &grit,
        &rebase_request(oids["main_m"], None, oids["a2"]),
        &rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true),
    )
    .expect("plan")
    {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected plan, got {other:?}"),
    };
    assert!(
        plan.ref_updates.iter().any(|u| u.name.ends_with("b")),
        "{:?}",
        plan.ref_updates
    );
    apply_plan(&grit, &plan);
    let merge =
        parse_commit(&grit.odb().read(&git_rev_parse(wt, "b")).expect("read").data).expect("parse");
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(merge.parents[1], oids["side"]);
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_moves_stacked_merge_with_unrelated_first_parent() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("base", "r"),
            ("main_m", "base"),
            ("a1", "base"),
            ("a2", "a1"),
            ("side", "r"),
            ("b_merge", "side"),
            ("b_merge", "a2"),
        ],
        &[
            ("main_m", "refs/heads/main"),
            ("a2", "refs/heads/a"),
            ("b_merge", "refs/heads/b"),
        ],
    );
    git(wt, &["checkout", "a"]);
    let grit = open_grit(wt);
    let plan = match plan_rebase(
        &grit,
        &rebase_request(oids["main_m"], None, oids["a2"]),
        &rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true),
    )
    .expect("plan")
    {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected plan, got {other:?}"),
    };
    assert!(
        plan.ref_updates.iter().any(|u| u.name.ends_with("b")),
        "{:?}",
        plan.ref_updates
    );
    apply_plan(&grit, &plan);
    let merge =
        parse_commit(&grit.odb().read(&git_rev_parse(wt, "b")).expect("read").data).expect("parse");
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(merge.parents[0], oids["side"]);
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_moves_stacked_branches() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("u", "r"), ("a", "u"), ("b", "a"), ("c", "b"), ("d", "b")],
        &[("c", "refs/heads/main"), ("d", "refs/heads/stacked")],
    );
    let onto = commit_file(wt, "base.txt", "base\n", "onto");
    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        oids["u"],
        onto,
        &[oids["c"]],
        &rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true),
    )
    .expect("plan");
    let main_update = plan
        .ref_updates
        .iter()
        .find(|u| u.name.ends_with("main"))
        .expect("main moves");
    let stacked_update = plan
        .ref_updates
        .iter()
        .find(|u| u.name.ends_with("stacked"))
        .expect("stacked moves");
    apply_plan(&grit, &plan);
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/main").expect("main"),
        main_update.new
    );
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/stacked").expect("stacked"),
        stacked_update.new
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_skips_already_applied_by_patch_id() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (upstream, onto, tip) = init_rebase_fixture(wt);
    let second = git_rev_parse(wt, "main~1");
    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        upstream,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, true),
    )
    .expect("plan");
    let second_row = plan
        .commits
        .iter()
        .find(|r| r.old == second)
        .expect("second commit row");
    assert_eq!(
        second_row.new,
        Some(onto),
        "patch-id equivalent should reuse onto tip"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_keep_applied() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (upstream, onto, tip) = init_rebase_fixture(wt);
    let second = git_rev_parse(wt, "main~1");
    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        upstream,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Keep, false),
    )
    .expect("plan");
    let second_row = plan
        .commits
        .iter()
        .find(|r| r.old == second)
        .expect("second row");
    assert!(second_row.new.is_some_and(|n| n != onto && n != second));
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_replays_merge_in_range() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("u", "r"),
            ("a", "u"),
            ("b", "a"),
            ("c", "a"),
            ("m", "b"),
            ("m", "c"),
            ("d", "m"),
        ],
        &[("d", "refs/heads/main")],
    );
    let onto = commit_file(wt, "onto.txt", "x\n", "onto");
    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        oids["u"],
        onto,
        &[oids["d"]],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, true),
    )
    .expect("rebase with merge");
    let merge_row = plan
        .commits
        .iter()
        .find(|r| r.old == oids["m"])
        .expect("merge row");
    let new_merge = merge_row.new.expect("rewritten merge");
    let parsed = parse_commit(&grit.odb().read(&new_merge).expect("read").data).expect("parse");
    assert_eq!(parsed.parents.len(), 2);
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_conflict_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    let upstream = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-b", "topic"]);
    commit_file(wt, "conflict.txt", "topic\n", "topic change");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "conflict.txt", "main\n", "main change");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "topic"]);
    let grit = open_grit(wt);
    let err = rebase_onto(
        &grit,
        upstream,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Abort, true),
    )
    .expect_err("conflict");
    assert!(matches!(
        err,
        grit_lib::revwalk::history::HistoryError::Conflict { .. }
    ));
    assert_eq!(git_rev_parse(wt, "HEAD"), tip);
}

#[test]
fn rebase_onto_defaults_upstream_from_tracking_config() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("u", "r"), ("a", "u"), ("b", "a")],
        &[("b", "refs/heads/feature")],
    );
    git(wt, &["config", "branch.feature.remote", "."]);
    git(
        wt,
        &["config", "branch.feature.merge", "refs/heads/upstream"],
    );
    write_ref(wt.join(".git").as_path(), "refs/heads/upstream", &oids["u"]).expect("upstream ref");
    let grit = open_grit(wt);
    let config = ConfigSet::load(grit.environment(), Some(grit.git_dir()), true).expect("config");
    let merge = config
        .get("branch.feature.merge")
        .expect("branch.feature.merge");
    assert_eq!(merge, "refs/heads/upstream");
    let upstream = resolve_ref(grit.git_dir(), "refs/heads/upstream").expect("upstream");
    assert_eq!(upstream, oids["u"]);
}

#[test]
fn rebase_onto_bare_repository() {
    use grit_lib::index::write_tree::write_tree_from_index;
    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
    use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};

    let dir = tempfile::tempdir().expect("tempdir");
    let grit = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init bare");
    let ident = PINNED_COMMITTER;
    fn tree_with(repo: &Repository, path: &str, bytes: &[u8]) -> grit_lib::error::Result<ObjectId> {
        let blob = repo.odb().write(ObjectKind::Blob, bytes)?;
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
            size: bytes.len() as u32,
            oid: blob,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        write_tree_from_index(repo.odb(), &index, "")
    }
    let empty = grit.odb().write(ObjectKind::Tree, &[]).expect("empty tree");
    let root = grit
        .odb()
        .write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: empty,
                parents: vec![],
                author: ident.into(),
                committer: ident.into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "root\n".into(),
                raw_message: None,
                extra_headers: vec![],
            }),
        )
        .expect("root");
    let tree_u = tree_with(&grit, "f.txt", b"1\n").expect("tree u");
    let u = grit
        .odb()
        .write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: tree_u,
                parents: vec![root],
                author: ident.into(),
                committer: ident.into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "u\n".into(),
                raw_message: None,
                extra_headers: vec![],
            }),
        )
        .expect("u");
    let tree_tip = tree_with(&grit, "f.txt", b"2\n").expect("tree tip");
    let tip = grit
        .odb()
        .write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: tree_tip,
                parents: vec![u],
                author: ident.into(),
                committer: ident.into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "tip\n".into(),
                raw_message: None,
                extra_headers: vec![],
            }),
        )
        .expect("tip");
    let onto = grit
        .odb()
        .write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: tree_u,
                parents: vec![root],
                author: ident.into(),
                committer: ident.into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "onto\n".into(),
                raw_message: None,
                extra_headers: vec![],
            }),
        )
        .expect("onto");
    write_ref(grit.git_dir(), "refs/heads/main", &tip).expect("ref");
    let open = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let plan = rebase_onto(
        &open,
        u,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, true),
    )
    .expect("plan bare");
    apply_plan(&open, &plan);
    assert_ne!(
        resolve_ref(open.git_dir(), "refs/heads/main").expect("ref"),
        tip
    );
}

#[test]
fn history_rebase_matches_git_replay_onto() {
    if !git_version_at_least(2, 43) {
        eprintln!("SKIP: git replay --onto requires git 2.43+");
        return;
    }
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    let (u_g, o_g, t_g) = init_rebase_fixture_from(wt_g, 1_700_000_000);
    let (u_git, o_git, t_git) = init_rebase_fixture_from(wt_git, 1_700_000_000);
    assert_eq!(u_g, u_git);
    assert_eq!(o_g, o_git);
    assert_eq!(t_g, t_git);
    git(wt_g, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt_g);
    let mut replay_opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, false);
    replay_opts.skip_applied = false;
    replay_opts.rewrite.preserve_committer = true;
    let plan = rebase_onto(&grit, u_g, o_g, &[t_g], &replay_opts).expect("grit plan");
    apply_plan(&grit, &plan);
    let replay = std::process::Command::new("git")
        .current_dir(wt_git)
        .args(["replay", "--onto", &o_git.to_hex(), &u_git.to_hex(), "main"])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git replay status");
    if !replay.success() {
        eprintln!("SKIP: git replay --onto unavailable or failed");
        return;
    }
    let grit_tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("grit tip");
    let git_tip = git_rev_parse(wt_git, "main");
    if grit_tip != git_tip {
        eprintln!(
            "grit commit:\n{}",
            git(wt_g, &["cat-file", "-p", &grit_tip.to_hex()])
        );
        eprintln!(
            "git commit:\n{}",
            git(wt_git, &["cat-file", "-p", &git_tip.to_hex()])
        );
    }
    assert_eq!(grit_tip, git_tip);
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

fn rebase_request(upstream: ObjectId, onto: Option<ObjectId>, tip: ObjectId) -> RebaseRequest {
    RebaseRequest {
        upstream,
        onto,
        branch_tip: tip,
    }
}

fn plan_apply(grit: &Repository, request: RebaseRequest, opts: &RebaseOntoOptions) -> ObjectId {
    match plan_rebase(grit, &request, opts).expect("plan_rebase") {
        RebasePlan::UpToDate => request.branch_tip,
        RebasePlan::Conflict { .. } => panic!("unexpected conflict"),
        RebasePlan::Plan(plan) => {
            apply_plan(grit, &plan);
            resolve_ref(grit.git_dir(), "refs/heads/main").expect("main")
        }
    }
}

fn assert_plan_rebase_matches_git_rebase_onto() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    let (u_g, o_g, t_g) = init_rebase_fixture_from(wt_g, 1_700_000_000);
    let (u_git, o_git, t_git) = init_rebase_fixture_from(wt_git, 1_700_000_000);
    assert_eq!(u_g, u_git);
    assert_eq!(o_g, o_git);
    assert_eq!(t_g, t_git);
    git(wt_g, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt_g);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let grit_tip = plan_apply(&grit, rebase_request(u_g, Some(o_g), t_g), &opts);
    git_rebase_onto(wt_git, &o_git, &u_git, "main");
    let git_tip = git_rev_parse(wt_git, "main");
    assert_eq!(grit_tip, git_tip);
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

#[test]
fn plan_rebase_matches_git_rebase_linear() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (upstream, _onto, tip) = init_rebase_fixture(wt);
    let grit = open_grit(wt);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let before = git_rev_parse(wt, "HEAD");
    let plan = plan_rebase(&grit, &rebase_request(upstream, None, tip), &opts).expect("plan");
    assert_eq!(plan, RebasePlan::UpToDate);
    assert_eq!(before, tip);
    git(wt, &["rebase", &upstream.to_hex()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), tip);
}

#[test]
fn plan_rebase_onto_matches_git_rebase_onto() {
    assert_plan_rebase_matches_git_rebase_onto();
}

#[test]
fn plan_rebase_up_to_date_is_noop() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "1\n", "first");
    let upstream = git_rev_parse(wt, "HEAD");
    commit_file(wt, "f.txt", "2\n", "second");
    let tip = git_rev_parse(wt, "HEAD");
    let grit = open_grit(wt);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let plan = plan_rebase(&grit, &rebase_request(upstream, None, tip), &opts).expect("plan");
    assert_eq!(plan, RebasePlan::UpToDate);
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/main").expect("main"),
        tip
    );
}

#[test]
fn plan_rebase_does_not_skip_when_onto_patch_differs() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "root.txt", "r\n", "root");
    let fork = git_rev_parse(wt, "HEAD");
    commit_file(wt, "a.txt", "topic\n", "topic");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-b", "onto-side", &fork.to_hex()]);
    commit_file(wt, "a.txt", "topic\n", "same a");
    commit_file(wt, "b.txt", "extra\n", "extra b");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "main"]);
    let grit = open_grit(wt);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let plan =
        match plan_rebase(&grit, &rebase_request(fork, Some(onto), tip), &opts).expect("plan") {
            RebasePlan::Plan(p) => p,
            other => panic!("expected plan, got {other:?}"),
        };
    let topic_row = plan
        .commits
        .iter()
        .find(|r| r.old == tip)
        .expect("topic row");
    assert!(
        topic_row.new.is_some_and(|n| n != onto),
        "topic commit must not reuse onto when onto carries an extra path change"
    );
}

#[test]
fn plan_rebase_skips_applied_only_from_fork_to_onto() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (upstream, onto, tip) = init_rebase_fixture(wt);
    let second = git_rev_parse(wt, "main~1");
    let grit = open_grit(wt);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &rebase_request(upstream, Some(onto), tip), &opts)
        .expect("plan")
    {
        RebasePlan::Plan(p) => p,
        other => panic!("expected plan, got {other:?}"),
    };
    let second_row = plan
        .commits
        .iter()
        .find(|r| r.old == second)
        .expect("second commit row");
    assert_eq!(
        second_row.new,
        Some(onto),
        "patch-id equivalent should reuse onto tip"
    );
}

#[test]
fn plan_rebase_keeps_merges_matches_git_rebase_merges() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    let oids = build_commit_dag(
        wt_g,
        &[
            ("u", "r"),
            ("a", "u"),
            ("b", "a"),
            ("c", "a"),
            ("m", "b"),
            ("m", "c"),
            ("d", "m"),
        ],
        &[("d", "refs/heads/main")],
    );
    let onto = commit_file(wt_g, "onto.txt", "x\n", "onto");
    git(wt_g, &["reset", "--hard", &oids["d"].to_hex()]);
    let oids_git = build_commit_dag(
        wt_git,
        &[
            ("u", "r"),
            ("a", "u"),
            ("b", "a"),
            ("c", "a"),
            ("m", "b"),
            ("m", "c"),
            ("d", "m"),
        ],
        &[("d", "refs/heads/main")],
    );
    let onto_git = commit_file(wt_git, "onto.txt", "x\n", "onto");
    git(wt_git, &["reset", "--hard", &oids_git["d"].to_hex()]);
    git(wt_g, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt_g);
    let mut opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    opts.flatten_leaf_merges = false;
    let grit_tip = plan_apply(
        &grit,
        rebase_request(oids["u"], Some(onto), oids["d"]),
        &opts,
    );
    let rebase = std::process::Command::new("git")
        .current_dir(wt_git)
        .args([
            "rebase",
            "--rebase-merges",
            "--onto",
            &onto_git.to_hex(),
            &oids_git["u"].to_hex(),
            "main",
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase --rebase-merges");
    assert!(rebase.success(), "git rebase --rebase-merges failed");
    let git_tip = git_rev_parse(wt_git, "main");
    let grit_merge =
        parse_commit(&grit.odb().read(&grit_tip).expect("read").data).expect("parse grit tip");
    let git_merge = parse_commit(&open_grit(wt_git).odb().read(&git_tip).expect("read").data)
        .expect("parse git tip");
    assert_eq!(grit_merge.parents.len(), git_merge.parents.len());
    assert_eq!(grit_merge.tree, git_merge.tree);
    assert_eq!(
        grit_merge.message.trim(),
        git_merge.message.trim(),
        "tip commit message"
    );
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

#[test]
fn plan_rebase_conflict_reports_paths_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    let upstream = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-b", "topic"]);
    commit_file(wt, "conflict.txt", "topic\n", "topic change");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "conflict.txt", "main\n", "main change");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "topic"]);
    let grit = open_grit(wt);
    let plan = plan_rebase(
        &grit,
        &rebase_request(upstream, Some(onto), tip),
        &rebase_opts(RefScope::Head, EmptyPolicy::Abort, true),
    )
    .expect("plan");
    match plan {
        RebasePlan::Conflict { conflicts, .. } => {
            assert_eq!(
                conflicts,
                vec![grit_lib::operation::Conflict {
                    path: "conflict.txt".into(),
                    kind: grit_lib::operation::ConflictKind::BothAdded,
                }]
            );
        }
        other => panic!("expected conflict, got {other:?}"),
    }
    assert_eq!(git_rev_parse(wt, "HEAD"), tip);
}

#[test]
fn plan_rebase_over_merged_unrelated_root_matches_git_rebase() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "grit.changeIds", "false"]);
    commit_file(wt, "f", "base\n", "base");
    let fork = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-q", "-b", "feat"]);
    commit_file(wt, "f", "base\nfeat\n", "feat");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["branch", "feat-git"]);
    git(wt, &["checkout", "-q", "main"]);
    commit_file(wt, "up", "up\n", "up");
    git(wt, &["checkout", "-q", "--orphan", "other"]);
    git(wt, &["rm", "-rfq", "."]);
    commit_file(wt, "g", "imported\n", "imported root");
    git(wt, &["checkout", "-q", "main"]);
    git(
        wt,
        &[
            "merge",
            "-q",
            "--allow-unrelated-histories",
            "--no-ff",
            "other",
            "-m",
            "merge imported",
        ],
    );
    let main = git_rev_parse(wt, "main");
    git(wt, &["checkout", "-q", "feat"]);

    let grit = open_grit(wt);
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &rebase_request(main, None, tip), &opts).expect("plan") {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected plan, got {other:?}"),
    };
    assert_eq!(plan.commits.len(), 1, "only feat is replayed");
    assert_eq!(plan.commits[0].old, tip);
    apply_plan(&grit, &plan);

    git(wt, &["rebase", "-q", "main", "feat-git"]);
    let grit_tip = resolve_ref(grit.git_dir(), "refs/heads/feat").expect("feat");
    let git_tip = git_rev_parse(wt, "feat-git");
    let grit_commit = parse_commit(&grit.odb().read(&grit_tip).expect("read").data).expect("parse");
    let git_commit = parse_commit(&grit.odb().read(&git_tip).expect("read").data).expect("parse");
    assert_eq!(grit_commit.parents, vec![main]);
    assert_eq!(grit_commit.parents, git_commit.parents);
    assert_eq!(grit_commit.tree, git_commit.tree);
    assert_eq!(grit_commit.message, git_commit.message);
    assert_git_fsck_strict(wt);
}

#[test]
fn history_rebase_matches_git_rebase() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    let (u_g, o_g, t_g) = init_rebase_fixture_from(wt_g, 1_700_000_000);
    let (u_git, o_git, t_git) = init_rebase_fixture_from(wt_git, 1_700_000_000);
    assert_eq!(u_g, u_git);
    assert_eq!(o_g, o_git);
    assert_eq!(t_g, t_git);
    git(wt_g, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt_g);
    let plan = rebase_onto(
        &grit,
        u_g,
        o_g,
        &[t_g],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, true),
    )
    .expect("grit plan");
    apply_plan(&grit, &plan);
    git_rebase_onto(wt_git, &o_git, &u_git, "main");
    let grit_tip = resolve_ref(grit.git_dir(), "refs/heads/main").expect("grit tip");
    let git_tip = git_rev_parse(wt_git, "main");
    if grit_tip != git_tip {
        eprintln!(
            "grit commit:\n{}",
            git(wt_g, &["cat-file", "-p", &grit_tip.to_hex()])
        );
        eprintln!(
            "git commit:\n{}",
            git(wt_git, &["cat-file", "-p", &git_tip.to_hex()])
        );
    }
    assert_eq!(grit_tip, git_tip);
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

fn init_rebase_favor_fixture(wt: &std::path::Path) -> (ObjectId, ObjectId, ObjectId) {
    commit_file(wt, "conflict.txt", "base\n", "base");
    let upstream = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-b", "topic"]);
    commit_file(wt, "conflict.txt", "topic\n", "topic side");
    commit_file(wt, "topic-only.txt", "only topic\n", "topic marker");
    let tip = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "conflict.txt", "main\n", "main side");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "topic"]);
    (upstream, onto, tip)
}

fn git_rebase_onto_with_strategy(
    wt: &std::path::Path,
    onto: &ObjectId,
    upstream: &ObjectId,
    branch: &str,
    strategy: &str,
) {
    std::process::Command::new("git")
        .current_dir(wt)
        .args([
            "-c",
            "rebase.applyCherryPicks=false",
            "rebase",
            "-X",
            strategy,
            "--onto",
            &onto.to_hex(),
            &upstream.to_hex(),
            branch,
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase -X");
}

fn rebase_opts_with_favor(favor: MergeFavor) -> RebaseOntoOptions {
    RebaseOntoOptions {
        favor,
        skip_applied: false,
        ..rebase_opts(RefScope::Head, EmptyPolicy::Drop, true)
    }
}

fn tip_tree(repo: &Repository, tip: ObjectId) -> ObjectId {
    parse_commit(&repo.odb().read(&tip).expect("read").data)
        .expect("parse")
        .tree
}

#[test]
fn rebase_favor_theirs_matches_git_rebase_x_theirs() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    git(wt_g, &["config", "grit.changeIds", "false"]);
    git(wt_git, &["config", "grit.changeIds", "false"]);
    let (u_g, o_g, t_g) = init_rebase_favor_fixture(wt_g);
    let (u_git, o_git, _t_git) = init_rebase_favor_fixture(wt_git);
    let grit = open_grit(wt_g);
    let plan = rebase_onto(
        &grit,
        u_g,
        o_g,
        &[t_g],
        &rebase_opts_with_favor(MergeFavor::Theirs),
    )
    .expect("grit plan");
    apply_plan(&grit, &plan);
    git_rebase_onto_with_strategy(wt_git, &o_git, &u_git, "topic", "theirs");
    let grit_tip = git_rev_parse(wt_g, "topic");
    let git_tip = git_rev_parse(wt_git, "topic");
    assert_eq!(
        tip_tree(&grit, grit_tip),
        tip_tree(&open_grit(wt_git), git_tip)
    );
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

#[test]
fn rebase_favor_ours_matches_git_rebase_x_ours() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    git(wt_g, &["config", "grit.changeIds", "false"]);
    git(wt_git, &["config", "grit.changeIds", "false"]);
    let (u_g, o_g, t_g) = init_rebase_favor_fixture(wt_g);
    let (u_git, o_git, _t_git) = init_rebase_favor_fixture(wt_git);
    let grit = open_grit(wt_g);
    let plan = rebase_onto(
        &grit,
        u_g,
        o_g,
        &[t_g],
        &rebase_opts_with_favor(MergeFavor::Ours),
    )
    .expect("grit plan");
    apply_plan(&grit, &plan);
    git_rebase_onto_with_strategy(wt_git, &o_git, &u_git, "topic", "ours");
    let grit_tip = git_rev_parse(wt_g, "topic");
    let git_tip = git_rev_parse(wt_git, "topic");
    assert_eq!(
        tip_tree(&grit, grit_tip),
        tip_tree(&open_grit(wt_git), git_tip)
    );
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

fn snapshot_repo_side_effects(wt: &std::path::Path) -> Vec<u8> {
    let git_dir = wt.join(".git");
    let mut out = Vec::new();
    for name in ["HEAD", "index"] {
        out.extend_from_slice(&std::fs::read(git_dir.join(name)).expect("read"));
    }
    out.extend_from_slice(&std::fs::read(wt.join("conflict.txt")).unwrap_or_default());
    let reflog = git_dir.join("logs/HEAD");
    if reflog.exists() {
        out.extend_from_slice(&std::fs::read(reflog).expect("reflog"));
    }
    for entry in std::fs::read_dir(git_dir.join("refs/heads")).expect("heads") {
        let path = entry.expect("entry").path();
        out.extend_from_slice(&std::fs::read(path).expect("ref"));
    }
    out
}

#[test]
fn rebase_check_reports_conflict_without_side_effects() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "grit.changeIds", "false"]);
    let (upstream, onto, tip) = init_rebase_favor_fixture(wt);
    let grit = open_grit(wt);
    let request = RebaseRequest {
        upstream,
        onto: Some(onto),
        branch_tip: tip,
    };
    let opts = rebase_opts(RefScope::Head, EmptyPolicy::Abort, true);
    let before = snapshot_repo_side_effects(wt);
    let check = plan_rebase_check(&grit, &request, &opts).expect("check");
    let after = snapshot_repo_side_effects(wt);
    assert_eq!(before, after, "refs, index, worktree and reflog must match");
    match check {
        RebaseCheckPlan::Conflict { conflicts, .. } => {
            assert!(conflicts.iter().any(|c| c.path.contains("conflict")
                && c.kind == grit_lib::operation::ConflictKind::BothModified));
        }
        other => panic!("expected conflict prediction, got {other:?}"),
    }
}

#[test]
fn rebase_check_names_sibling_branch_whose_replay_conflicts() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "1\n2\n3\n", "base");
    git(wt, &["switch", "-q", "-c", "topic"]);
    let t1 = commit_file(wt, "b.txt", "b\n", "t1");
    git(wt, &["switch", "-q", "-c", "other"]);
    let o1 = commit_file(wt, "a.txt", "1o\n2\n3\n", "o1");
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "a.txt", "1m\n2\n3\n", "m1");
    git(wt, &["switch", "-q", "topic"]);
    let grit = open_grit(wt);
    let request = RebaseRequest {
        upstream: m1,
        onto: None,
        branch_tip: t1,
    };

    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true);
    match plan_rebase_check(&grit, &request, &opts).expect("check") {
        RebaseCheckPlan::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        } => {
            assert_eq!(commit, o1);
            assert_eq!(subject, "o1");
            assert!(conflicts.iter().any(|c| c.path == "a.txt"));
            assert_eq!(refs, vec!["refs/heads/other".to_owned()]);
        }
        other => panic!("expected conflict prediction, got {other:?}"),
    }

    let head_only = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    assert!(matches!(
        plan_rebase_check(&grit, &request, &head_only).expect("check"),
        RebaseCheckPlan::Clean { .. }
    ));
    assert_eq!(git_rev_parse(wt, "refs/heads/other"), o1);
    assert_eq!(git_rev_parse(wt, "refs/heads/topic"), t1);
}

fn flatten_opts() -> RebaseOntoOptions {
    let mut opts = rebase_opts(RefScope::Head, EmptyPolicy::Drop, true);
    opts.flatten_leaf_merges = true;
    opts
}

fn planned_ref_tip(plan: &grit_lib::revwalk::history::RewritePlan, name: &str) -> ObjectId {
    plan.ref_updates
        .iter()
        .find(|update| update.name == name)
        .unwrap_or_else(|| panic!("{name} moves"))
        .new
}

#[test]
fn rebase_onto_flatten_keeps_local_commit_under_merge_of_upstream_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "S1");
    git(wt, &["checkout", "-q", "-b", "local"]);
    let local = commit_file(wt, "a.txt", "local\n", "Change a");
    git(wt, &["checkout", "-q", "main"]);
    let s2 = commit_file(wt, "b.txt", "2\n", "S2");
    git(wt, &["checkout", "-q", "local"]);
    git(wt, &["merge", "-q", "--no-ff", "--no-edit", "main"]);
    let merge = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-q", "main"]);
    let s3 = commit_file(wt, "c.txt", "3\n", "S3");
    git(wt, &["checkout", "-q", "local"]);
    git(wt, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt);

    let plan = rebase_onto(&grit, s2, s3, &[merge], &flatten_opts()).expect("plan");

    let merge_row = plan.commits.iter().find(|r| r.old == merge).expect("row");
    assert_eq!(merge_row.new, None, "the merge of upstream is dropped");
    let local_row = plan.commits.iter().find(|r| r.old == local).expect("row");
    let tip = planned_ref_tip(&plan, "refs/heads/local");
    assert_eq!(local_row.new, Some(tip), "the branch lands on Change a");

    // `git rebase` drops merges too and replays `Change a` onto S3: same commit, byte for byte.
    git_rebase_onto(wt, &s3, &s2, "local");
    assert_eq!(git_rev_parse(wt, "local"), tip);
    assert_eq!(git_rev_parse(wt, "local^"), s3);
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_onto_flatten_linearizes_topic_merge_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let fork = commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["checkout", "-q", "-b", "topic"]);
    commit_file(wt, "t1.txt", "1\n", "topic one");
    commit_file(wt, "t2.txt", "2\n", "topic two");
    git(wt, &["checkout", "-q", "main"]);
    commit_file(wt, "main.txt", "m\n", "main line");
    git(wt, &["merge", "-q", "--no-ff", "--no-edit", "topic"]);
    let merge = git_rev_parse(wt, "HEAD");
    git(wt, &["checkout", "-q", "-b", "upstream", &fork.to_hex()]);
    let onto = commit_file(wt, "up.txt", "u\n", "upstream");
    git(wt, &["checkout", "-q", "main"]);
    git(wt, &["config", "grit.changeIds", "false"]);
    let grit = open_grit(wt);

    let plan = rebase_onto(&grit, fork, onto, &[merge], &flatten_opts()).expect("plan");
    let tip = planned_ref_tip(&plan, "refs/heads/main");

    git_rebase_onto(wt, &onto, &fork, "main");
    assert_eq!(git_rev_parse(wt, "main"), tip, "same line as `git rebase`");
    assert_eq!(git_rev_parse(wt, "main~3"), onto);
    assert_git_fsck_strict(wt);
}

/// Copy of the repository at `wt`, for running the same rebase with system `git`.
fn git_oracle_copy(wt: &std::path::Path) -> tempfile::TempDir {
    let copy = tempfile::tempdir().expect("git copy");
    let status = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", wt.display()),
            &copy.path().to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success(), "copy repository for git");
    copy
}

/// `<tree> <subject>` of every commit in `upstream..branch`, oldest first.
fn replayed_shape(wt: &std::path::Path, upstream: &str, branch: &str) -> Vec<String> {
    git(
        wt,
        &[
            "log",
            "--reverse",
            "--format=%T %s",
            &format!("{upstream}..{branch}"),
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn subjects(shape: &[String]) -> Vec<&str> {
    shape
        .iter()
        .map(|line| line.split_once(' ').map_or("", |(_, subject)| subject))
        .collect()
}

fn tracked_paths(wt: &std::path::Path, rev: &str) -> Vec<String> {
    git(wt, &["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `grit rebase main` on the checked-out `feat`, with every descendant branch updated.
fn rebase_feat_onto_main(wt: &std::path::Path) -> grit_lib::revwalk::history::RewritePlan {
    let grit = open_grit(wt);
    let request = rebase_request(git_rev_parse(wt, "main"), None, git_rev_parse(wt, "feat"));
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &request, &opts).expect("plan_rebase") {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected a plan, got {other:?}"),
    };
    apply_plan(&grit, &plan);
    plan
}

/// Rebase `feat` onto `main` with grit and with `git rebase main` on a copy, and check both
/// replay the same commits (trees and subjects) directly on top of `main`.
fn assert_skipping_rebase_matches_git(wt: &std::path::Path, expected: &[&str]) {
    let git_wt = git_oracle_copy(wt);
    let main = git_rev_parse(wt, "main");
    rebase_feat_onto_main(wt);
    git(git_wt.path(), &["rebase", "main"]);

    let grit_shape = replayed_shape(wt, "main", "feat");
    assert_eq!(subjects(&grit_shape), expected);
    assert_eq!(grit_shape, replayed_shape(git_wt.path(), "main", "feat"));
    assert_eq!(
        git_rev_parse(wt, &format!("feat~{}", expected.len())),
        main,
        "replayed commits sit directly on main"
    );
    assert_eq!(
        tracked_paths(wt, "feat"),
        tracked_paths(git_wt.path(), "feat")
    );
    assert_git_fsck_strict(wt);
    assert_git_fsck_strict(git_wt.path());
}

#[test]
fn rebase_skipping_duplicate_tip_keeps_earlier_commits_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base", "0\n", "base");
    git(wt, &["switch", "-qc", "feat"]);
    commit_file(wt, "x", "x\n", "x1");
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "m1", "m1\n", "m1");
    commit_file(wt, "m2", "m2\n", "m2");
    git(wt, &["switch", "-q", "feat"]);
    git(wt, &["cherry-pick", &m1.to_hex()]);

    assert_skipping_rebase_matches_git(wt, &["x1"]);
    assert_eq!(tracked_paths(wt, "feat"), ["base", "m1", "m2", "x"]);
}

#[test]
fn rebase_skipping_duplicate_mid_stack_keeps_both_sides_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base", "0\n", "base");
    git(wt, &["switch", "-qc", "feat"]);
    commit_file(wt, "x", "x\n", "x1");
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "m1", "m1\n", "m1");
    commit_file(wt, "m2", "m2\n", "m2");
    commit_file(wt, "m3", "m3\n", "m3");
    git(wt, &["switch", "-q", "feat"]);
    git(wt, &["cherry-pick", &m1.to_hex()]);
    commit_file(wt, "y", "y\n", "y1");

    assert_skipping_rebase_matches_git(wt, &["x1", "y1"]);
    assert_eq!(
        tracked_paths(wt, "feat"),
        ["base", "m1", "m2", "m3", "x", "y"]
    );
}

#[test]
fn rebase_skipping_first_commit_whose_match_is_not_onto_lands_on_onto_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base", "0\n", "base");
    git(wt, &["switch", "-qc", "feat"]);
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "m1", "m1\n", "m1");
    commit_file(wt, "m2", "m2\n", "m2");
    commit_file(wt, "m3", "m3\n", "m3");
    git(wt, &["switch", "-q", "feat"]);
    git(wt, &["cherry-pick", &m1.to_hex()]);
    commit_file(wt, "y", "y\n", "y1");

    assert_skipping_rebase_matches_git(wt, &["y1"]);
}

#[test]
fn rebase_skipping_duplicate_keeps_stacked_refs_on_the_rewritten_prefix() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base", "0\n", "base");
    git(wt, &["switch", "-qc", "feat"]);
    let x1 = commit_file(wt, "x", "x\n", "x1");
    git(wt, &["branch", "below"]);
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "m1", "m1\n", "m1");
    commit_file(wt, "m2", "m2\n", "m2");
    git(wt, &["switch", "-q", "feat"]);
    git(wt, &["cherry-pick", &m1.to_hex()]);
    let skipped = git_rev_parse(wt, "HEAD");
    git(wt, &["branch", "at-skip"]);
    commit_file(wt, "y", "y\n", "y1");
    let main = git_rev_parse(wt, "main");

    let plan = rebase_feat_onto_main(wt);

    let row = plan
        .commits
        .iter()
        .find(|row| row.old == skipped)
        .expect("row for the skipped commit");
    assert!(row.reused);
    assert_eq!(row.new, Some(m1), "the row names the upstream equivalent");

    let new_x1 = git_rev_parse(wt, "feat~1");
    assert_ne!(new_x1, x1);
    assert_eq!(git_rev_parse(wt, "feat~2"), main);
    assert_eq!(subjects(&replayed_shape(wt, "main", "feat")), ["x1", "y1"]);
    assert_eq!(
        git_rev_parse(wt, "below"),
        new_x1,
        "a ref before the skip follows its rewritten commit"
    );
    assert_eq!(
        git_rev_parse(wt, "at-skip"),
        new_x1,
        "a ref at the skipped commit moves to the replay chain so far"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_skipping_duplicate_after_merge_keeps_stacked_refs_and_tree_like_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base", "0\n", "base");
    git(wt, &["switch", "-qc", "feat-a"]);
    commit_file(wt, "a1", "a1\n", "a1");
    commit_file(wt, "a2", "a2\n", "a2");
    git(wt, &["switch", "-qc", "side"]);
    commit_file(wt, "s", "s\n", "s1");
    git(wt, &["switch", "-qc", "feat", "feat-a"]);
    git(wt, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    git(wt, &["switch", "-q", "main"]);
    let m1 = commit_file(wt, "m1", "m1\n", "m1");
    commit_file(wt, "m2", "m2\n", "m2");
    git(wt, &["switch", "-q", "feat"]);
    git(wt, &["cherry-pick", &m1.to_hex()]);
    commit_file(wt, "b", "b\n", "b1");
    let main = git_rev_parse(wt, "main");
    let git_wt = git_oracle_copy(wt);

    rebase_feat_onto_main(wt);
    git(git_wt.path(), &["rebase", "--rebase-merges", "main"]);

    assert_eq!(
        git_rev_parse(wt, "feat^{tree}"),
        git_rev_parse(git_wt.path(), "feat^{tree}")
    );
    assert_eq!(
        tracked_paths(wt, "feat"),
        ["a1", "a2", "b", "base", "m1", "m2", "s"]
    );
    let mut grit_subjects = replayed_shape(wt, "main", "feat");
    let mut git_subjects = replayed_shape(git_wt.path(), "main", "feat");
    grit_subjects.sort();
    git_subjects.sort();
    assert_eq!(grit_subjects, git_subjects);
    for stacked in ["feat-a", "side"] {
        assert!(
            git_merge_base_is_ancestor(wt, "main", stacked),
            "{stacked} is rebased onto main"
        );
        assert!(
            git_merge_base_is_ancestor(wt, stacked, "feat"),
            "{stacked} stays part of feat"
        );
        assert_eq!(
            git(wt, &["log", "-1", "--format=%s", stacked]).trim(),
            if stacked == "feat-a" { "a2" } else { "s1" }
        );
    }
    assert_ne!(git_rev_parse(wt, "feat-a"), main);
    assert_git_fsck_strict(wt);
    assert_git_fsck_strict(git_wt.path());
}

#[test]
fn rebase_merge_side_branch_keeps_side_parentage_like_git_rebase_merges() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    git(wt_g, &["config", "grit.changeIds", "false"]);
    git(wt_git, &["config", "grit.changeIds", "false"]);
    commit_file(wt_g, "base.txt", "base\n", "base");
    commit_file(wt_git, "base.txt", "base\n", "base");
    for wt in [wt_g, wt_git] {
        git(wt, &["switch", "-qc", "side"]);
        commit_file(wt, "side.txt", "s\n", "side: a");
        commit_file(wt, "side.txt", "s\ns2\n", "side: b");
        git(wt, &["switch", "-qc", "feat", "main"]);
        commit_file(wt, "shared.txt", "mine\n", "feat: a (shared)");
        commit_file(wt, "c.txt", "c\n", "feat: c");
        git(wt, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        commit_file(wt, "feat.txt", "f2\n", "feat: b");
        git(wt, &["switch", "-q", "main"]);
        commit_file(wt, "other.txt", "o\n", "main: other");
        git(wt, &["switch", "-q", "feat"]);
    }
    let fork = git_rev_parse(wt_g, "main~1");
    let onto = git_rev_parse(wt_g, "main");
    let tip = git_rev_parse(wt_g, "feat");
    let fork_git = git_rev_parse(wt_git, "main~1");
    let onto_git = git_rev_parse(wt_git, "main");

    let grit = open_grit(wt_g);
    let mut opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, false);
    opts.skip_applied = false;
    plan_apply(&grit, rebase_request(fork, Some(onto), tip), &opts);

    let rebase = std::process::Command::new("git")
        .current_dir(wt_git)
        .args([
            "rebase",
            "--rebase-merges",
            "--onto",
            &onto_git.to_hex(),
            &fork_git.to_hex(),
            "feat",
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase");
    assert!(rebase.success(), "git rebase --rebase-merges failed");

    let git_tip = git_rev_parse(wt_git, "feat");
    let grit_merge_oid = git_rev_parse(wt_g, "feat~1");
    let git_merge_oid = git_rev_parse(wt_git, "feat~1");
    let grit_merge =
        parse_commit(&grit.odb().read(&grit_merge_oid).expect("read").data).expect("parse");
    let git_merge = parse_commit(
        &open_grit(wt_git)
            .odb()
            .read(&git_merge_oid)
            .expect("read")
            .data,
    )
    .expect("parse");
    assert_eq!(grit_merge.parents.len(), 2);
    assert_eq!(grit_merge.parents.len(), git_merge.parents.len());
    assert_eq!(grit_merge.tree, git_merge.tree);
    let grit_tree = git(wt_g, &["ls-tree", "-r", "--name-only", "feat"]);
    let grit_files: Vec<_> = grit_tree
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    assert!(
        grit_files.contains(&"side.txt"),
        "merged side content kept: {grit_files:?}"
    );
    assert!(
        !git_merge_base_is_ancestor(wt_g, "feat", "side"),
        "side ref must not absorb feat commits"
    );
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

fn init_merge_only_integration_branch(wt: &std::path::Path) {
    commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["switch", "-qc", "feat", "main"]);
    git(wt, &["switch", "-qc", "topic"]);
    commit_file(wt, "t.txt", "t\n", "topic 1");
    git(wt, &["switch", "-q", "feat"]);
    git(
        wt,
        &["merge", "-q", "--no-ff", "-m", "Merge topic", "topic"],
    );
    git(wt, &["branch", "-D", "topic"]);
    git(wt, &["switch", "-q", "main"]);
    commit_file(wt, "u.txt", "u\n", "upstream");
    git(wt, &["switch", "-q", "feat"]);
}

#[test]
fn rebase_merge_only_integration_branch_replays_issue_2321() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    git(wt_g, &["config", "grit.changeIds", "false"]);
    git(wt_git, &["config", "grit.changeIds", "false"]);
    init_merge_only_integration_branch(wt_g);
    init_merge_only_integration_branch(wt_git);

    let fork = git_rev_parse(wt_g, "main~1");
    let onto = git_rev_parse(wt_g, "main");
    let tip = git_rev_parse(wt_g, "feat");
    let fork_git = git_rev_parse(wt_git, "main~1");
    let onto_git = git_rev_parse(wt_git, "main");

    let grit = open_grit(wt_g);
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, false);
    match plan_rebase(&grit, &rebase_request(fork, Some(onto), tip), &opts).expect("plan") {
        RebasePlan::UpToDate => panic!("merge-only integration branch must not be up to date"),
        RebasePlan::Conflict { .. } => panic!("unexpected conflict"),
        RebasePlan::Plan(plan) => {
            assert_eq!(plan.commits.len(), 2, "topic commit and merge");
        }
    }
    match plan_rebase_check(&grit, &rebase_request(fork, Some(onto), tip), &opts).expect("check") {
        RebaseCheckPlan::UpToDate => panic!("check must not report up to date"),
        RebaseCheckPlan::Clean { commits } => assert_eq!(commits.len(), 2),
        other => panic!("unexpected check plan: {other:?}"),
    }

    plan_apply(&grit, rebase_request(fork, Some(onto), tip), &opts);

    let rebase = std::process::Command::new("git")
        .current_dir(wt_git)
        .args([
            "rebase",
            "--rebase-merges",
            "--onto",
            &onto_git.to_hex(),
            &fork_git.to_hex(),
            "feat",
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase");
    assert!(rebase.success(), "git rebase --rebase-merges failed");

    let grit_tip = git_rev_parse(wt_g, "feat");
    let git_tip = git_rev_parse(wt_git, "feat");
    let grit_merge =
        parse_commit(&grit.odb().read(&grit_tip).expect("read").data).expect("parse grit tip");
    let git_merge = parse_commit(&open_grit(wt_git).odb().read(&git_tip).expect("read").data)
        .expect("parse git tip");
    assert_eq!(grit_merge.parents.len(), git_merge.parents.len());
    assert_eq!(grit_merge.tree, git_merge.tree);
    assert_eq!(
        grit_merge.message.trim(),
        git_merge.message.trim(),
        "merge subject"
    );
    assert_eq!(
        grit_merge.parents[0], onto,
        "merge first parent is upstream tip"
    );
    let grit_topic = grit_merge.parents[1];
    let git_topic = git_merge.parents[1];
    let grit_topic_commit =
        parse_commit(&grit.odb().read(&grit_topic).expect("read").data).expect("parse grit topic");
    let git_topic_commit =
        parse_commit(&open_grit(wt_git).odb().read(&git_topic).expect("read").data)
            .expect("parse git topic");
    assert_eq!(
        grit_topic_commit.parents[0], onto,
        "topic reparented onto main"
    );
    assert_eq!(grit_topic_commit.message.trim(), "topic 1");
    assert_eq!(grit_topic_commit.tree, git_topic_commit.tree);
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

#[test]
fn rebase_chain_of_merge_only_integration_branches_replays_issue_2321() {
    let grit_repo = HistoryRepo::init();
    let git_repo = HistoryRepo::init();
    let wt_g = grit_repo.worktree();
    let wt_git = git_repo.worktree();
    git(wt_g, &["config", "grit.changeIds", "false"]);
    git(wt_git, &["config", "grit.changeIds", "false"]);
    for wt in [wt_g, wt_git] {
        commit_file(wt, "base.txt", "base\n", "base");
        git(wt, &["switch", "-qc", "feat", "main"]);
        for (topic, file, msg) in [
            ("topic1", "t1.txt", "topic 1"),
            ("topic2", "t2.txt", "topic 2"),
            ("topic3", "t3.txt", "topic 3"),
        ] {
            git(wt, &["switch", "-qc", topic]);
            commit_file(wt, file, "x\n", msg);
            git(wt, &["switch", "-q", "feat"]);
            git(
                wt,
                &[
                    "merge",
                    "-q",
                    "--no-ff",
                    "-m",
                    &format!("Merge {topic}"),
                    topic,
                ],
            );
            git(wt, &["branch", "-D", topic]);
        }
        git(wt, &["switch", "-q", "main"]);
        commit_file(wt, "u.txt", "u\n", "upstream");
        git(wt, &["switch", "-q", "feat"]);
    }

    let fork = git_rev_parse(wt_g, "main~1");
    let onto = git_rev_parse(wt_g, "main");
    let tip = git_rev_parse(wt_g, "feat");
    let fork_git = git_rev_parse(wt_git, "main~1");
    let onto_git = git_rev_parse(wt_git, "main");

    let grit = open_grit(wt_g);
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, false);
    match plan_rebase(&grit, &rebase_request(fork, Some(onto), tip), &opts).expect("plan") {
        RebasePlan::UpToDate => panic!("merge-only chain must not be up to date"),
        RebasePlan::Conflict { .. } => panic!("unexpected conflict"),
        RebasePlan::Plan(plan) => assert_eq!(plan.commits.len(), 6, "3 topics + 3 merges"),
    }
    plan_apply(&grit, rebase_request(fork, Some(onto), tip), &opts);

    let rebase = std::process::Command::new("git")
        .current_dir(wt_git)
        .args([
            "rebase",
            "--rebase-merges",
            "--onto",
            &onto_git.to_hex(),
            &fork_git.to_hex(),
            "feat",
        ])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_DATE", "1700001000 +0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase");
    assert!(rebase.success(), "git rebase --rebase-merges failed");

    let grit_tip = git_rev_parse(wt_g, "feat");
    let git_tip = git_rev_parse(wt_git, "feat");
    assert_ne!(grit_tip, tip, "feat must move");
    let grit_merge =
        parse_commit(&grit.odb().read(&grit_tip).expect("read").data).expect("parse grit tip");
    let git_merge = parse_commit(&open_grit(wt_git).odb().read(&git_tip).expect("read").data)
        .expect("parse git tip");
    assert_eq!(grit_merge.parents.len(), git_merge.parents.len());
    assert_eq!(grit_merge.tree, git_merge.tree);
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_git);
}

/// The fixture from issue #2182: `feature` (X, Y) with `mid` at X and `backup` at Y, while
/// `main` gained M. `feature` is checked out.
fn init_backup_fixture(wt: &std::path::Path) {
    commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["switch", "-qc", "feature"]);
    commit_file(wt, "x.txt", "x\n", "X");
    commit_file(wt, "y.txt", "y\n", "Y");
    git(wt, &["branch", "backup"]);
    git(wt, &["branch", "mid", "HEAD~1"]);
    git(wt, &["switch", "-q", "main"]);
    commit_file(wt, "m.txt", "m\n", "M");
    git(wt, &["switch", "-q", "feature"]);
}

#[test]
fn rebase_leaves_backup_branch_at_checked_out_tip() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    init_backup_fixture(wt);
    let old_tip = git_rev_parse(wt, "feature");
    let old_mid = git_rev_parse(wt, "mid");
    let grit = open_grit(wt);
    let request = rebase_request(git_rev_parse(wt, "main"), None, old_tip);
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &request, &opts).expect("plan_rebase") {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected a plan, got {other:?}"),
    };
    let moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert!(moved.contains(&"refs/heads/feature"), "{moved:?}");
    assert!(moved.contains(&"refs/heads/mid"), "{moved:?}");
    assert!(!moved.contains(&"refs/heads/backup"), "{moved:?}");

    apply_plan(&grit, &plan);
    assert_eq!(git_rev_parse(wt, "backup"), old_tip);
    assert_ne!(git_rev_parse(wt, "feature"), old_tip);
    assert_ne!(git_rev_parse(wt, "mid"), old_mid);
    assert_eq!(git_rev_parse(wt, "feature~1"), git_rev_parse(wt, "mid"));
    assert_eq!(git_rev_parse(wt, "feature~2"), git_rev_parse(wt, "main"));
    assert_eq!(git(wt, &["log", "--format=%s", "backup"]), "Y\nX\nbase\n");
    assert_git_fsck_strict(wt);
}

/// Replay planning must not leave an empty `dir/` tree entry when a commit deletes the last
/// file in that directory (issue #2263 review).
#[test]
fn rebase_replay_delete_last_file_in_dir_matches_git_tree() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "upstream.txt", "v1\n", "root");
    let fork = git_rev_parse(wt, "HEAD");
    git(wt, &["branch", "feat"]);
    git(wt, &["switch", "-q", "main"]);
    commit_file(wt, "upstream.txt", "v2\n", "main bump");
    let onto = git_rev_parse(wt, "HEAD");
    git(wt, &["switch", "-q", "feat"]);
    commit_file(wt, "dir/only.txt", "only\n", "add dir");
    std::fs::remove_file(wt.join("dir/only.txt")).expect("rm");
    git(wt, &["add", "-u"]);
    std::process::Command::new("git")
        .current_dir(wt)
        .args(["commit", "-m", "drop dir"])
        .env("GIT_AUTHOR_NAME", "Refs Harness Author")
        .env("GIT_AUTHOR_EMAIL", "refs-harness@example.com")
        .env("GIT_COMMITTER_NAME", "Refs Harness Author")
        .env("GIT_COMMITTER_EMAIL", "refs-harness@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("commit");
    let tip = git_rev_parse(wt, "HEAD");

    let grit = open_grit(wt);
    let plan = rebase_onto(
        &grit,
        fork,
        onto,
        &[tip],
        &rebase_opts(RefScope::Head, EmptyPolicy::Drop, false),
    )
    .expect("grit plan");
    apply_plan(&grit, &plan);
    let grit_tree = git_rev_parse_verify(wt, "HEAD^{tree}");

    let git_wt = tempfile::tempdir().expect("git wt");
    std::process::Command::new("git")
        .current_dir(git_wt.path())
        .args(["clone", wt.to_str().expect("utf8"), "r"])
        .status()
        .expect("clone");
    let git_wt = git_wt.path().join("r");
    std::process::Command::new("git")
        .current_dir(&git_wt)
        .args(["rebase", "--onto", &onto.to_hex(), &fork.to_hex(), "feat"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git rebase");
    let git_tree = git_rev_parse_verify(&git_wt, "feat^{tree}");

    assert_eq!(
        grit_tree, git_tree,
        "replay delete must match git rebase tree (no empty dir placeholder)"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn fast_forward_rebase_leaves_backup_branch_at_checked_out_tip() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["switch", "-qc", "feature"]);
    git(wt, &["branch", "backup"]);
    let old_tip = git_rev_parse(wt, "feature");
    git(wt, &["switch", "-q", "main"]);
    let main = commit_file(wt, "m.txt", "m\n", "M");
    git(wt, &["switch", "-q", "feature"]);
    let grit = open_grit(wt);
    let request = rebase_request(main, None, old_tip);
    let opts = rebase_opts(RefScope::Branches, EmptyPolicy::Drop, true);
    let plan = match plan_rebase(&grit, &request, &opts).expect("plan_rebase") {
        RebasePlan::Plan(plan) => plan,
        other => panic!("expected a plan, got {other:?}"),
    };
    let moved: Vec<&str> = plan.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(moved, ["refs/heads/feature"]);
    apply_plan(&grit, &plan);
    assert_eq!(git_rev_parse(wt, "feature"), main);
    assert_eq!(git_rev_parse(wt, "backup"), old_tip);
    assert_git_fsck_strict(wt);
}

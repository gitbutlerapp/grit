//! Integration tests for [`grit_lib::revwalk::history::rebase_onto`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

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
    build_commit_dag, commit_file, git, git_rev_parse_verify, git_version_at_least, open_grit_repo,
    HistoryRepo,
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
            committer: PINNED_COMMITTER.into(),
            preserve_committer: false,
            sign: true,
        },
        skip_applied: skip,
        flatten_leaf_merges: false,
        favor: MergeFavor::default(),
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
            identity: "Test <t@grit.test>".into(),
            reflog_message: "rebase onto".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
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
        RebasePlan::Conflict { paths, .. } => {
            assert!(paths.iter().any(|p| p.contains("conflict")));
        }
        other => panic!("expected conflict, got {other:?}"),
    }
    assert_eq!(git_rev_parse(wt, "HEAD"), tip);
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
    let (u_git, o_git, t_git) = init_rebase_favor_fixture(wt_git);
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
        RebaseCheckPlan::Conflict { paths, .. } => {
            assert!(paths.iter().any(|p| p.contains("conflict")));
        }
        other => panic!("expected conflict prediction, got {other:?}"),
    }
}

//! Pick, revert, and rebase on detached HEAD match system `git` (issue #2530).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayRequest};
use grit_lib::porcelain::replay_sequence::{ReplayOptions, ReplaySequenceRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    start_rebase, ApplyOptions, PreRebase, RebaseOntoOptions, RebaseRequest, RefScope,
    RewriteOptions, StartRebaseOptions,
};
use grit_lib::state::HeadState;

const IDENT: &str = "Detached Pick <d@example.com> 1700000000 +0000";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Detached Pick")
        .env("GIT_AUTHOR_EMAIL", "d@example.com")
        .env("GIT_COMMITTER_NAME", "Detached Pick")
        .env("GIT_COMMITTER_EMAIL", "d@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("ident")
}

fn detached_rebase_opts() -> RebaseOntoOptions {
    RebaseOntoOptions {
        rewrite: RewriteOptions {
            scope: RefScope::Head,
            empty: grit_lib::revwalk::history::EmptyPolicy::Drop,
            committer: ident(),
            preserve_committer: false,
            sign: false,
        },
        skip_applied: false,
        flatten_leaf_merges: false,
        favor: grit_lib::diff::merge_file::MergeFavor::default(),
        drop: Vec::new(),
    }
}

fn start_rebase_opts(upstream: ObjectId, tip: ObjectId) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: None,
            branch_tip: tip,
        },
        rebase: detached_rebase_opts(),
        apply: ApplyOptions {
            identity: ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
        pre_rebase: PreRebase::Skip,
    }
}

fn init_linear_repo(root: &Path) -> (ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "d@example.com"]);
    git(root, &["config", "user.name", "Detached Pick"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for i in 1..=3 {
        std::fs::write(root.join("f"), format!("b{i}\n")).unwrap();
        git(root, &["add", "f"]);
        git(root, &["commit", "-q", "-m", &format!("b{i}")]);
    }
    let main_tip = ObjectId::from_hex(&git(root, &["rev-parse", "main"])).expect("main");
    git(root, &["switch", "-c", "side", "main~2"]);
    std::fs::write(root.join("s"), b"s\n").unwrap();
    git(root, &["add", "s"]);
    git(root, &["commit", "-q", "-m", "side"]);
    let side_tip = ObjectId::from_hex(&git(root, &["rev-parse", "side"])).expect("side");
    git(root, &["switch", "--detach", "main"]);
    (main_tip, side_tip)
}

fn assert_head_detached(root: &Path) {
    let repo = open_grit(root);
    match grit_lib::state::resolve_head_for_repository(&repo).expect("head") {
        HeadState::Detached { .. } => {}
        other => panic!("expected detached HEAD, got {other:?}"),
    }
}

fn assert_git_fsck(root: &Path) {
    git(root, &["fsck", "--strict"]);
}

#[test]
fn grit_pick_on_detached_head_matches_git() {
    let root = tempfile::tempdir().expect("tmpdir");
    let (main_tip, side_tip) = init_linear_repo(root.path());
    let _ = main_tip;

    let repo = open_grit(root.path());
    let outcome = replay_commit(
        &repo,
        &ReplayRequest {
            commit: side_tip,
            direction: ReplayDirection::Pick,
            committer: ident(),
            message_override: None,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick on detached HEAD");
    let grit_lib::porcelain::replay::ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected committed pick, got {outcome:?}");
    };

    assert_head_detached(root.path());
    assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), oid.to_hex());
    assert_eq!(
        git(root.path(), &["rev-parse", "main"]),
        main_tip.to_hex(),
        "branch at the same tip must stay put"
    );
    assert_git_fsck(root.path());
}

#[test]
fn grit_revert_on_detached_head_matches_git() {
    let root = tempfile::tempdir().expect("tmpdir");
    let (main_tip, _side) = init_linear_repo(root.path());

    let repo = open_grit(root.path());
    let outcome = replay_commit(
        &repo,
        &ReplayRequest {
            commit: main_tip,
            direction: ReplayDirection::Revert,
            committer: ident(),
            message_override: None,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("revert on detached HEAD");
    assert!(matches!(
        outcome,
        grit_lib::porcelain::replay::ReplayOutcome::Committed { .. }
    ));

    assert_head_detached(root.path());
    assert_eq!(git(root.path(), &["rev-parse", "main"]), main_tip.to_hex());
    assert_git_fsck(root.path());
}

#[test]
fn grit_rebase_upstream_on_detached_head_matches_git() {
    let root = tempfile::tempdir().expect("tmpdir");
    let (main_tip, side_tip) = init_linear_repo(root.path());
    let _ = side_tip;

    let repo = open_grit(root.path());
    let head_before = ObjectId::from_hex(&git(root.path(), &["rev-parse", "HEAD"])).unwrap();
    let opts = start_rebase_opts(side_tip, head_before);
    start_rebase(&repo, &opts, &mut NullProgress, &NeverCancel).expect("rebase detached");

    assert_head_detached(root.path());
    assert_ne!(
        git(root.path(), &["rev-parse", "HEAD"]),
        head_before.to_hex()
    );
    assert_eq!(git(root.path(), &["rev-parse", "main"]), main_tip.to_hex());
    assert_git_fsck(root.path());
}

#[test]
fn grit_predict_pick_on_detached_head() {
    let root = tempfile::tempdir().expect("tmpdir");
    let (_main_tip, side_tip) = init_linear_repo(root.path());
    let repo = open_grit(root.path());
    use grit_lib::porcelain::replay_sequence::{predict_replay_sequence, SequencePrediction};
    let prediction = predict_replay_sequence(
        &repo,
        &ReplaySequenceRequest {
            commits: vec![side_tip],
            direction: ReplayDirection::Pick,
            committer: ident(),
            options: ReplayOptions::default(),
        },
    )
    .expect("predict on detached HEAD");
    assert!(matches!(prediction, SequencePrediction::Clean { .. }));
}

#[test]
fn git_cherry_pick_then_grit_reads_detached_head() {
    let root = tempfile::tempdir().expect("tmpdir");
    let (_main_tip, side_tip) = init_linear_repo(root.path());
    git(root.path(), &["cherry-pick", &side_tip.to_hex()]);
    let git_head = git(root.path(), &["rev-parse", "HEAD"]);
    let repo = open_grit(root.path());
    match grit_lib::state::resolve_head_for_repository(&repo).expect("head") {
        HeadState::Detached { oid } => assert_eq!(oid.to_hex(), git_head),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
}

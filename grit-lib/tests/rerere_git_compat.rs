//! Rerere records written by grit must be readable by `git rerere` (t4200 core).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::rerere::{repo_rerere, rerere_enabled, RerereAutoupdate, RerereEventKind};
use grit_lib::merge::{ContinueOptions, IntegrateBranchOutcome, MergeCommitMode, MergeOptions};
use grit_lib::merge::{ReplayOptions, ReplaySequenceRequest, SequenceOutcome};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::CommitRequest;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    RebaseOntoOptions, RebaseOutcome, RebaseRequest, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::state::{resolve_head, HeadState};
use grit_test_support::start_replay_sequence;
use grit_test_support::{continue_merge, integrate_branch, integrate_branch_with};
use tempfile::TempDir;

fn git_cmd(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_oid(root: &std::path::Path, spec: &str) -> ObjectId {
    let hex = git_cmd(root, &["rev-parse", spec]).trim().to_string();
    ObjectId::from_hex(&hex).expect("oid")
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main", "."]);
    git_cmd(dir.path(), &["config", "rerere.enabled", "true"]);
    dir
}

fn open_grit(dir: &TempDir) -> Repository {
    Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open")
}

fn head_oid(repo: &Repository) -> ObjectId {
    match repo.references().head().expect("head") {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => panic!("invalid head"),
    }
}

fn merge_rr_len(dir: &TempDir) -> usize {
    std::fs::metadata(dir.path().join(".git/MERGE_RR"))
        .map(|m| m.len() as usize)
        .unwrap_or(0)
}

fn rr_cache_has_postimage(dir: &TempDir) -> bool {
    let cache = dir.path().join(".git/rr-cache");
    std::fs::read_dir(cache).ok().is_some_and(|rd| {
        rd.filter_map(Result::ok)
            .any(|ent| ent.path().is_dir() && ent.path().join("postimage").is_file())
    })
}

fn rr_cache_entries(dir: &TempDir) -> usize {
    std::fs::read_dir(dir.path().join(".git/rr-cache"))
        .map(|rd| {
            rd.filter(|e| {
                e.as_ref()
                    .map(|x| x.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0)
}

fn setup_diverged_branches(dir: &TempDir) -> ObjectId {
    std::fs::write(dir.path().join("f"), b"l1\nl2\nl3\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.path().join("f"), b"l1\nSIDE\nl3\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "side"]);
    git_cmd(dir.path(), &["checkout", "-q", "main"]);
    std::fs::write(dir.path().join("f"), b"l1\nMAIN\nl3\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "main"]);
    git_oid(dir.path(), "side")
}

fn git_record_rerere_resolution(dir: &TempDir) {
    let merge = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "side"])
        .output()
        .expect("merge");
    assert!(!merge.status.success());
    std::fs::write(dir.path().join("f"), b"l1\nRESOLVED\nl3\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "merged"]);
    assert_eq!(rr_cache_entries(dir), 1, "git should record rr-cache entry");
    git_cmd(dir.path(), &["reset", "-q", "--hard", "HEAD~1"]);
}

fn ident() -> grit_lib::ident::Signature {
    "T <t@e.com> 1700000000 +0000".parse().expect("ident")
}

fn continue_opts() -> ContinueOptions {
    ContinueOptions {
        message: Some("merged".into()),
        author: Some(ident()),
        committer: Some(ident()),
    }
}

#[test]
fn t4200_rerere_records_readable_by_git() {
    let dir = init_repo();
    std::fs::write(dir.path().join("f.txt"), b"base\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.path().join("f.txt"), b"side change\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "side"]);
    git_cmd(dir.path(), &["checkout", "-q", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"main change\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "main"]);
    let merge_out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "side"])
        .output()
        .expect("merge");
    assert!(!merge_out.status.success(), "expect merge conflict");

    let repo = open_grit(&dir);
    let cfg = repo.config().snapshot().expect("cfg");
    assert!(rerere_enabled(cfg.as_ref(), repo.git_dir()));
    let events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere");
    assert!(!events.is_empty(), "grit should record rerere preimage");

    let status = git_cmd(dir.path(), &["rerere", "status"]);
    assert!(
        status.contains("f.txt"),
        "git rerere status should list conflict path, got {status:?}"
    );
}

#[test]
fn issue_2468_git_rr_cache_replayed_by_grit_merge() {
    let dir = init_repo();
    let side = setup_diverged_branches(&dir);
    git_record_rerere_resolution(&dir);

    let repo = open_grit(&dir);
    let head = head_oid(&repo);
    let outcome = integrate_branch(
        &repo,
        "main",
        head,
        side,
        "side",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");

    let IntegrateBranchOutcome::Conflicts { rerere, .. } = outcome else {
        panic!("expected merge conflicts with rerere replay");
    };
    assert!(
        rerere
            .iter()
            .any(|e| e.path == "f" && e.kind == RerereEventKind::ResolvedFromPrevious),
        "grit merge should replay git-recorded resolution"
    );
    let content = std::fs::read_to_string(dir.path().join("f")).expect("f");
    assert_eq!(content, "l1\nRESOLVED\nl3\n");
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    assert!(
        index.has_unmerged_entries(),
        "replayed resolution stays unstaged without autoupdate"
    );
}

#[test]
fn issue_2468_grit_records_rr_cache_for_git_merge() {
    let dir = init_repo();
    let side = setup_diverged_branches(&dir);

    let repo = open_grit(&dir);
    let head = head_oid(&repo);
    integrate_branch(
        &repo,
        "main",
        head,
        side,
        "side",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");

    std::fs::write(dir.path().join("f"), b"l1\nRESOLVED\nl3\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    continue_merge(
        &repo,
        &continue_opts(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("continue merge");

    assert_eq!(
        rr_cache_entries(&dir),
        1,
        "grit merge --continue should record rr-cache"
    );

    git_cmd(dir.path(), &["reset", "-q", "--hard", "HEAD~1"]);
    let merge = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge");
    let stderr = String::from_utf8_lossy(&merge.stderr);
    assert!(
        merge.status.success() || stderr.contains("Resolved"),
        "git merge with rerere: {stderr}"
    );
    let content = std::fs::read_to_string(dir.path().join("f")).expect("f");
    assert_eq!(content, "l1\nRESOLVED\nl3\n");
}

#[test]
fn issue_2468_rerere_disabled_leaves_markers() {
    let dir = init_repo();
    let side = setup_diverged_branches(&dir);
    git_record_rerere_resolution(&dir);
    git_cmd(dir.path(), &["config", "rerere.enabled", "false"]);

    let repo = open_grit(&dir);
    let head = head_oid(&repo);
    let outcome = integrate_branch(
        &repo,
        "main",
        head,
        side,
        "side",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    let content = std::fs::read_to_string(dir.path().join("f")).expect("f");
    assert!(content.contains("<<<<<<<"), "conflict markers remain");
    assert_eq!(rr_cache_entries(&dir), 1, "existing git cache unchanged");
}

#[test]
fn issue_2468_pick_replays_git_rr_cache() {
    let dir = init_repo();
    setup_diverged_branches(&dir);
    git_record_rerere_resolution(&dir);
    let pick_oid = git_oid(dir.path(), "side");

    let repo = open_grit(&dir);
    let req = ReplaySequenceRequest {
        commits: vec![pick_oid],
        direction: ReplayDirection::Pick,
        committer: ident(),
        options: ReplayOptions::default(),
    };
    let out =
        start_replay_sequence(&repo, &req, &mut NullProgress, &CancelToken::never()).expect("pick");
    let SequenceOutcome::Conflicts { rerere, .. } = out else {
        panic!("expected pick conflicts");
    };
    assert!(
        rerere
            .iter()
            .any(|e| e.kind == RerereEventKind::ResolvedFromPrevious),
        "pick should replay rerere"
    );
}

#[test]
fn issue_2468_rebase_replays_git_rr_cache() {
    let dir = init_repo();
    setup_diverged_branches(&dir);
    git_record_rerere_resolution(&dir);
    git_cmd(dir.path(), &["checkout", "-q", "side"]);

    let repo = open_grit(&dir);
    let upstream = git_oid(dir.path(), "main");
    let tip = git_oid(dir.path(), "HEAD");
    let opts = StartRebaseOptions::new(
        RebaseRequest::new(upstream, tip),
        RebaseOntoOptions::new(
            RewriteOptions::new(ident())
                .with_scope(RefScope::Head)
                .with_sign(false),
        )
        .with_skip_applied(false)
        .with_favor(Default::default()),
        grit_lib::revwalk::history::ApplyOptions::new(ident(), "rebase"),
    );
    let out = repo
        .history()
        .start_rebase(
            &opts,
            repo.command_runner().as_ref(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rebase");
    let RebaseOutcome::Stopped(stop) = out else {
        panic!("expected rebase stop on conflicts");
    };
    assert!(
        stop.rerere
            .iter()
            .any(|e| e.kind == RerereEventKind::ResolvedFromPrevious),
        "rebase should replay rerere"
    );
}

#[test]
fn issue_2468_squash_conflict_commit_records_rerere_postimage() {
    let dir = init_repo();
    let side = setup_diverged_branches(&dir);

    let repo = open_grit(&dir);
    let head = head_oid(&repo);
    let outcome = integrate_branch_with(
        &repo,
        "main",
        head,
        side,
        "side",
        &MergeOptions::default().with_commit(MergeCommitMode::Squash),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("squash merge");

    let IntegrateBranchOutcome::Conflicts { .. } = outcome else {
        panic!("expected squash merge conflicts");
    };
    assert!(
        !repo.git_dir().join("MERGE_HEAD").exists(),
        "squash conflict must not write MERGE_HEAD"
    );
    assert!(repo.git_dir().join("SQUASH_MSG").is_file());

    std::fs::write(dir.path().join("f"), b"l1\nRESOLVED\nl3\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);

    let mut req = CommitRequest::new("squashed side");
    req.author = Some(ident());
    req.committer = Some(ident());
    let outcome = repo
        .commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit");
    assert!(
        outcome
            .rerere
            .iter()
            .any(|e| e.path == "f" && e.kind == RerereEventKind::RecordedResolution),
        "squash conflict commit should record rerere postimage"
    );
    assert_eq!(merge_rr_len(&dir), 0, "MERGE_RR should be cleared");
    assert!(
        rr_cache_has_postimage(&dir),
        "rr-cache should hold postimage"
    );
    assert_eq!(rr_cache_entries(&dir), 1);
}

#[test]
fn issue_2468_merge_continue_returns_recorded_resolution() {
    let dir = init_repo();
    let side = setup_diverged_branches(&dir);

    let repo = open_grit(&dir);
    let head = head_oid(&repo);
    integrate_branch(
        &repo,
        "main",
        head,
        side,
        "side",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");

    std::fs::write(dir.path().join("f"), b"l1\nRESOLVED\nl3\n").unwrap();
    git_cmd(dir.path(), &["add", "f"]);
    let cont = continue_merge(
        &repo,
        &continue_opts(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("continue merge");
    assert!(
        cont.rerere
            .iter()
            .any(|e| e.kind == RerereEventKind::RecordedResolution),
        "merge continue should return recorded_resolution events"
    );
}

//! HEAD reflog entries while a rebase is stopped (issue #2560).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::progress::{CancelToken, NullProgress};

mod support;

use grit_lib::objects::ObjectId;
use grit_lib::revwalk::history::{
    RebaseOntoOptions, RebaseOutcome, RebaseRequest, RefScope, RewriteOptions, StartRebaseOptions,
};
use support::{git, git_rev_parse_verify, open_grit_repo, HistoryRepo};

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";
const COMMITTER: &str = "Refs Harness Author <refs-harness@example.com> 1700001000 +0000";

fn test_ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("IDENT")
}

fn git_oid(wt: &std::path::Path, spec: &str) -> ObjectId {
    git_rev_parse_verify(wt, spec).expect(spec)
}

fn commit_file(wt: &std::path::Path, path: &str, contents: &str, msg: &str) {
    std::fs::write(wt.join(path), contents).expect("write");
    git(wt, &["add", path]);
    git(wt, &["commit", "-qm", msg]);
}

fn conflict_rebase_fixture() -> (HistoryRepo, ObjectId, ObjectId, ObjectId, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");
    let orig_tip = git_oid(wt, "topic");
    (repo, upstream, onto, tip, orig_tip)
}

fn start_opts(upstream: ObjectId, onto: Option<ObjectId>, tip: ObjectId) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto,
            branch_tip: tip,
        },
        rebase: RebaseOntoOptions::new(
            RewriteOptions::new(COMMITTER.parse().expect("signature"))
                .with_scope(RefScope::Head)
                .with_sign(false),
        )
        .with_skip_applied(false),
        apply: grit_lib::revwalk::history::ApplyOptions::new(test_ident(), "rebase: onto")
            .with_record_operation(Some("rebase".into())),
        exec: None,
    }
}

fn assert_head_reflog_chain(wt: &std::path::Path) {
    let head = git_oid(wt, "HEAD");
    assert_eq!(git_oid(wt, "HEAD@{0}"), head, "HEAD@{{0}} must be HEAD");
    let log = std::fs::read_to_string(wt.join(".git/logs/HEAD")).expect("HEAD log");
    let entries: Vec<(&str, &str)> = log
        .lines()
        .map(|line| {
            let (header, _) = line.split_once('\t').expect("reflog message");
            let mut oids = header.split(' ');
            (oids.next().expect("old"), oids.next().expect("new"))
        })
        .collect();
    for pair in entries.windows(2) {
        assert_eq!(pair[1].0, pair[0].1, "HEAD reflog chain broken: {log}");
    }
}

#[test]
fn conflict_stop_logs_head_detach() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit_repo(wt);
    git(wt, &["switch", "topic"]);
    let out = grit
        .history()
        .start_rebase(
            &start_opts(upstream, Some(onto), tip),
            grit.command_runner().as_ref(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("start");
    assert!(matches!(out, RebaseOutcome::Stopped(_)));
    assert_eq!(
        grit.references().resolve("refs/heads/topic").unwrap(),
        orig_tip
    );
    assert!(matches!(
        grit.references().head().expect("head"),
        grit_lib::state::HeadState::Detached { .. }
    ));
    assert_head_reflog_chain(wt);
    let head = git_oid(wt, "HEAD");
    let log = std::fs::read_to_string(wt.join(".git/logs/HEAD")).expect("HEAD log");
    let last = log.lines().last().expect("entry");
    assert!(
        last.starts_with(&format!("{} {} ", orig_tip.to_hex(), head.to_hex())),
        "detach must log pre-rebase tip → replay parent: {last}"
    );
    assert!(last.contains("\trebase: onto"));
}

#[test]
fn conflict_stop_abort_logs_head_reattach() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit_repo(wt);
    git(wt, &["switch", "topic"]);
    assert!(matches!(
        grit.history()
            .start_rebase(
                &start_opts(upstream, Some(onto), tip),
                grit.command_runner().as_ref(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never()
            )
            .expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    let detached = git_oid(wt, "HEAD");
    assert_head_reflog_chain(wt);
    let detach_line = std::fs::read_to_string(wt.join(".git/logs/HEAD"))
        .expect("HEAD log")
        .lines()
        .last()
        .expect("detach")
        .to_owned();
    grit.history().abort_rebase(&test_ident()).expect("abort");
    assert_eq!(
        grit.references().head().unwrap().branch_name(),
        Some("topic")
    );
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_head_reflog_chain(wt);
    assert_eq!(git_oid(wt, "HEAD@{0}"), orig_tip);
    let log = std::fs::read_to_string(wt.join(".git/logs/HEAD")).expect("HEAD log");
    let abort_line = log.lines().last().expect("abort");
    assert!(
        abort_line.starts_with(&format!("{} {} ", detached.to_hex(), orig_tip.to_hex())),
        "abort must log detached → restored tip: {abort_line}"
    );
    assert!(abort_line.contains("\trebase (abort)"));
    assert_eq!(
        log.lines().nth(log.lines().count() - 2),
        Some(detach_line.as_str())
    );
}

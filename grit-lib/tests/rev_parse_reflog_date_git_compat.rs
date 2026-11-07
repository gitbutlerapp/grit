//! Reflog `@{date}` selectors vs system `git rev-parse` (issue #2224).

use crate::support;

use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diagnostics::{CollectingDiagnostics, Warning};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use support::HistoryRepo;

fn git_commit_dated(worktree: &std::path::Path, content: &str, msg: &str, epoch: i64) {
    fs::write(worktree.join("f"), content).expect("write f");
    let date = format!("{epoch} +0000");
    assert!(Command::new("git")
        .current_dir(worktree)
        .args(["add", "f"])
        .status()
        .expect("git add")
        .success());
    assert!(Command::new("git")
        .current_dir(worktree)
        .args(["commit", "-qm", msg])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .status()
        .expect("git commit")
        .success());
}

fn git_rev_parse_reflog_date(worktree: &std::path::Path, spec: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "--verify", spec])
        .output()
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse {spec}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("git oid")
}

#[test]
fn reflog_date_selectors_match_git_relaxed_grammar() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    Command::new("git")
        .current_dir(wt)
        .args(["branch", "-M", "main"])
        .status()
        .expect("rename main");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    for off in [5_i64, 3, 1, 0] {
        let t = now - off * 86_400 - 3_600;
        git_commit_dated(wt, &off.to_string(), &format!("c-{off}-days-ago"), t);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");

    for date_inner in [
        "2.days.ago",
        "2 days ago",
        "yesterday",
        "last week",
        "noon yesterday",
        "midnight",
        "Oct 27",
        "10/27/2026",
        "last month",
    ] {
        let spec = format!("main@{{{date_inner}}}");
        let git_oid = git_rev_parse_reflog_date(wt, &spec);
        let grit_oid = grit_repo
            .revwalk()
            .resolve(&spec, &grit_lib::revwalk::ResolveOptions::default())
            .unwrap_or_else(|e| {
                panic!("grit failed to resolve {spec:?}: {e}");
            });
        assert_eq!(
            git_oid, grit_oid,
            "mismatch for {spec}: git {git_oid} grit {grit_oid}"
        );
    }
}

#[test]
fn reflog_date_older_than_oldest_emits_warning_and_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    Command::new("git")
        .current_dir(wt)
        .args(["branch", "-M", "main"])
        .status()
        .expect("rename main");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    git_commit_dated(wt, "1", "recent", now - 3_600);

    let spec = "main@{last year}";
    let git_oid = git_rev_parse_reflog_date(wt, spec);

    let sink = Arc::new(CollectingDiagnostics::new());
    let options = RepositoryOptions::empty().with_diagnostics(sink.clone());
    let grit_repo = Repository::discover(Some(wt), &options).expect("open");
    let grit_oid = grit_repo
        .revwalk()
        .resolve(spec, &grit_lib::revwalk::ResolveOptions::default())
        .expect("resolve last year");
    assert_eq!(git_oid, grit_oid);

    let warnings = sink.warnings();
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, Warning::ReflogDateBeforeOldestEntry { .. })),
        "expected ReflogDateBeforeOldestEntry warning, got {warnings:?}"
    );
}

//! `grit status` after a rewrite apply was killed between its ref updates and its checkout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use grit_lib::progress::CancelToken;
use grit_lib::revwalk::history::{
    ApplyOptions, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
};
use grit_test_support::configure_git_env;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(repo: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd.args(args).current_dir(repo).output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn grit(repo: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.args(args).current_dir(repo).output().expect("grit")
}

/// A repository whose tip was rewritten to change `a.txt`, with refs moved and the checkout
/// killed after writing half of the new `a.txt`.
fn interrupted_rewrite_repo() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    git(wt, &["init", "-q", "-b", "main"]);
    fs::write(wt.join("a.txt"), "old\n").expect("write");
    git(wt, &["add", "a.txt"]);
    git(wt, &["commit", "-q", "-m", "a"]);
    let tip = git(wt, &["rev-parse", "HEAD"]);

    let index = wt.join(".git/rewrite-index");
    let index_env = index.to_str().expect("utf8 path");
    let blob = {
        fs::write(wt.join("new-a"), "new content\n").expect("write");
        let blob = git(wt, &["hash-object", "-w", "new-a"]);
        fs::remove_file(wt.join("new-a")).expect("rm");
        blob
    };
    let tree = {
        let run = |args: &[&str]| {
            let mut cmd = Command::new("git");
            configure_git_env(&mut cmd);
            let out = cmd
                .env("GIT_INDEX_FILE", index_env)
                .args(args)
                .current_dir(wt)
                .output()
                .expect("git");
            assert!(out.status.success());
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        run(&["read-tree", "HEAD"]);
        run(&[
            "update-index",
            "--cacheinfo",
            &format!("100644,{blob},a.txt"),
        ]);
        run(&["write-tree"])
    };
    fs::remove_file(&index).expect("rm temp index");

    let repo = grit_lib::repo::Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover");
    let mut edits = HistoryEdits::new();
    edits.insert(
        tip.parse().expect("oid"),
        CommitEdit::rewrite(
            Some(tree.parse().expect("tree oid")),
            Some("rewritten\n".into()),
            None,
            None,
        ),
    );
    let plan = repo
        .history()
        .plan_rewrite(
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: "A <a@b.c> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: false,
            },
        )
        .expect("plan");
    let new_tip = plan.ref_updates[0].new.to_hex();
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(true);
    repo.history()
        .apply(
            &plan,
            &ApplyOptions::new(
                grit_lib::ident::parse_signature("A <a@b.c> 1 +0000").expect("ident"),
                "rewrite",
            )
            .with_hooks(grit_lib::command_runner::hooks::PostOperationHooks::Skip),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect_err("injected abort after refs");
    grit_lib::test_support::inject::set_test_inject_rewrite_apply_abort_after_refs(false);
    fs::write(wt.join("a.txt"), "new con").expect("half-written checkout");
    (tmp, new_tip)
}

fn assert_finished(wt: &Path, new_tip: &str) {
    assert_eq!(git(wt, &["rev-parse", "HEAD"]), new_tip);
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).expect("read"),
        "new content\n"
    );
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
}

#[test]
fn status_human_reports_finished_interrupted_rewrite_once() {
    let (tmp, new_tip) = interrupted_rewrite_repo();
    let wt = tmp.path();
    let out = grit(wt, &["status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.starts_with(
            "Finished the checkout of an interrupted rebase or history edit; HEAD stays on the rewritten commits.\n"
        ),
        "{stdout}"
    );
    assert_finished(wt, &new_tip);

    let again = grit(wt, &["status"]);
    assert!(again.status.success());
    assert!(!String::from_utf8_lossy(&again.stdout).contains("interrupted"));
}

#[test]
fn status_json_reports_finished_interrupted_rewrite() {
    let (tmp, new_tip) = interrupted_rewrite_repo();
    let wt = tmp.path();
    let out = grit(wt, &["--json", "status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["interrupted_rewrite"], "finished_checkout");
    assert_eq!(json["head"], new_tip.as_str());
    assert_eq!(json["clean"], true);
    assert_finished(wt, &new_tip);

    let again: serde_json::Value =
        serde_json::from_slice(&grit(wt, &["--json", "status"]).stdout).expect("json");
    assert!(again.get("interrupted_rewrite").is_none(), "{again}");
}

#[test]
fn status_markdown_reports_interrupted_rewrite_pending_while_index_is_locked() {
    let (tmp, new_tip) = interrupted_rewrite_repo();
    let wt = tmp.path();
    let lock = wt.join(".git/index.lock");
    fs::write(&lock, "").expect("lock");
    let out = grit(wt, &["--markdown", "status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.starts_with("> An interrupted rebase or history edit is pending"),
        "{stdout}"
    );
    assert_eq!(git(wt, &["rev-parse", "HEAD"]), new_tip);

    fs::remove_file(&lock).expect("unlock");
    let out = grit(wt, &["--markdown", "status"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("> Finished the checkout"));
    assert_finished(wt, &new_tip);
}

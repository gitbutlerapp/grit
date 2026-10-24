//! History rewrites re-sign replacement commits when signing is configured.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::command_runner::RecordingRunner;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, reword, ApplyOptions, EmptyPolicy, HistoryError, RefScope, RewriteOptions,
};
use grit_test_support::git;

use support::{build_commit_dag, git_fsck_strict, HistoryRepo};

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn configure_ssh_signing(root: &Path, key: &Path, allowed: &Path) {
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "gpg.format", "ssh"]);
    git(
        root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).expect("read pub");
    std::fs::write(allowed, format!("t@example.com {pub_key}")).expect("allowed_signers");
    git(
        root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );
}

fn rewrite_opts(committer: &str, sign: bool) -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: committer.to_owned(),
        preserve_committer: false,
        sign,
    }
}

fn open_grit(wt: &Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn open_grit_with_runner(wt: &Path, runner: Arc<RecordingRunner>) -> Repository {
    let opts = RepositoryOptions::empty().with_command_runner(runner);
    Repository::discover(Some(wt), &opts).expect("discover")
}

fn count_signature_headers(raw: &str) -> usize {
    let header = raw.split("\n\n").next().unwrap_or(raw);
    header
        .lines()
        .filter(|line| line.starts_with("gpgsig ") || line.starts_with("gpgsig-sha256 "))
        .count()
}

fn signed_tip_via_grit(root: &Path, gpgsign: bool) -> ObjectId {
    git(
        root,
        &[
            "config",
            "commit.gpgsign",
            if gpgsign { "true" } else { "false" },
        ],
    );
    std::fs::write(root.join("signed.txt"), b"signed\n").expect("write");
    let repo = open_grit(root);
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let ident = "T <t@example.com> 1700000001 +0000".to_owned();
    create_commit(
        &repo,
        &CommitRequest {
            message: "signed subject\n".to_owned(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect("create_commit")
    .oid
}

#[test]
fn history_rewrite_resigns_when_gpgsign() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = root.join("key");
    let allowed = root.join("allowed_signers");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");

    git(root, &["init", "-q", "-b", "main", "."]);
    configure_ssh_signing(root, &key, &allowed);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("base.txt"), b"base\n").expect("write");
    git(root, &["add", "base.txt"]);
    git(
        root,
        &[
            "commit",
            "-m",
            "base",
            "--author",
            "T <t@example.com>",
            "--date",
            "1700000000 +0000",
        ],
    );

    let signed = signed_tip_via_grit(root, true);
    git(root, &["update-ref", "refs/heads/main", &signed.to_hex()]);

    let grit = open_grit(root);
    let plan = reword(
        &grit,
        signed,
        "reworded signed subject\n".into(),
        &rewrite_opts("Rewriter <r@grit.test> 1700000002 +0000", true),
    )
    .expect("reword plan");
    apply_rewrite(
        &grit,
        &plan,
        &ApplyOptions {
            identity: "Rewriter <r@grit.test>".into(),
            reflog_message: "history reword".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("apply");

    assert_eq!(git_out(root, &["log", "-1", "--format=%G?"]), "G");
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    let change_pos = cat.find("change-id ").expect("change-id header");
    let sig_pos = cat.find("gpgsig ").expect("gpgsig header");
    assert!(change_pos < sig_pos, "change-id must precede gpgsig");
    git_out(root, &["verify-commit", "HEAD"]);
    git_fsck_strict(root);
}

#[test]
fn history_rewrite_unsigned_when_gpgsign_off() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = root.join("key");
    let allowed = root.join("allowed_signers");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");

    git(root, &["init", "-q", "-b", "main", "."]);
    configure_ssh_signing(root, &key, &allowed);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("f.txt"), b"f\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(
        root,
        &[
            "commit",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
            "--date",
            "1700000000 +0000",
        ],
    );

    let signed = signed_tip_via_grit(root, true);
    git(root, &["config", "commit.gpgsign", "false"]);
    let grit = open_grit(root);
    let plan = reword(
        &grit,
        signed,
        "unsigned rewrite\n".into(),
        &rewrite_opts("Rewriter <r@grit.test> 1700000002 +0000", true),
    )
    .expect("reword");
    let new_oid = plan.replacement(signed).expect("replacement");
    let raw = grit.odb().read(&new_oid).expect("read").data;
    let parsed = parse_commit(&raw).expect("parse");
    assert_eq!(count_signature_headers(&String::from_utf8_lossy(&raw)), 0);
    assert!(
        parsed.message.contains("unsigned rewrite"),
        "message should be updated"
    );
}

#[test]
fn history_dry_run_does_not_invoke_signer() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let key = wt.join("key");
    let allowed = wt.join("allowed_signers");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    configure_ssh_signing(wt, &key, &allowed);
    git(wt, &["config", "commit.gpgsign", "true"]);

    let oids = build_commit_dag(wt, &[], &[("a", "refs/heads/main")]);
    let runner = RecordingRunner::always_success();
    let grit = open_grit_with_runner(wt, runner.clone());
    reword(
        &grit,
        oids["a"],
        "dry-run reword\n".into(),
        &rewrite_opts("Rewriter <r@grit.test> 1700000002 +0000", false),
    )
    .expect("dry-run plan");
    assert!(
        runner.specs().is_empty(),
        "signing must not spawn external commands during dry-run planning"
    );
}

#[test]
fn signing_failure_aborts_rewrite_without_ref_moves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("f.txt"), b"f\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(
        root,
        &[
            "commit",
            "-m",
            "tip",
            "--author",
            "T <t@example.com>",
            "--date",
            "1700000000 +0000",
        ],
    );
    git(root, &["config", "commit.gpgsign", "true"]);
    git(
        root,
        &["config", "user.signingkey", "/nonexistent/signing-key"],
    );

    let git_dir = root.join(".git");
    let tip_before = resolve_ref(&git_dir, "refs/heads/main").expect("resolve tip");

    let grit = open_grit(root);
    let err = reword(
        &grit,
        tip_before,
        "should fail signing\n".into(),
        &rewrite_opts("Rewriter <r@grit.test> 1700000002 +0000", true),
    )
    .expect_err("signing should fail");
    assert!(
        matches!(err, HistoryError::Signing { .. }),
        "expected Signing error, got {err:?}"
    );
    let tip_after = resolve_ref(&git_dir, "refs/heads/main").expect("resolve tip");
    assert_eq!(
        tip_before, tip_after,
        "refs must not move when signing fails"
    );
}

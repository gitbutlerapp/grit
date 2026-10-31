//! `reference-transaction` hook around ref store updates (issue #2220).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use grit_lib::refs::{
    commit_ref_update_on_store, update_branch_for_commit_on_store, BranchCommitRefUpdate,
};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn install_reference_transaction_hook(repo: &Repository, log_path: &Path, deny_prepared: bool) {
    let hooks = repo.git_dir().join("hooks");
    fs::create_dir_all(&hooks).expect("hooks dir");
    let deny = if deny_prepared { "1" } else { "0" };
    let script = format!(
        "#!/bin/sh\n\
         echo \"$1: $(cat | tr '\\n' '|')\" >> {log}\n\
         [ \"{deny}\" = 1 ] && [ \"$1\" = prepared ] && exit 1\n\
         exit 0\n",
        log = log_path.display()
    );
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, script).expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn branch_create_runs_prepared_and_committed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("rt.log");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "a@b.c"]);
    git(tmp.path(), &["config", "user.name", "n"]);
    fs::write(tmp.path().join("f"), b"x").expect("file");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);

    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    install_reference_transaction_hook(&repo, &log, false);

    let tip = repo.refs().resolve("HEAD").expect("head");
    fs::write(&log, "").expect("clear log");
    commit_ref_update_on_store(
        repo.refs(),
        RefUpdate {
            name: "refs/heads/topic".into(),
            new_value: Some(RawRef::Direct(tip)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        },
    )
    .expect("create branch");

    let text = read_log(&log);
    assert!(text.contains("prepared:"), "{text}");
    assert!(text.contains("committed:"), "{text}");
    assert!(text.contains("refs/heads/topic"), "{text}");
    assert!(
        text.contains(&format!(
            "{} {}",
            ObjectId::null(repo.odb().hash_algo()).to_hex(),
            tip.to_hex()
        )),
        "{text}"
    );
}

#[test]
fn prepared_refusal_leaves_ref_unchanged_and_logs_aborted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("rt.log");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "a@b.c"]);
    git(tmp.path(), &["config", "user.name", "n"]);
    fs::write(tmp.path().join("f"), b"x").expect("file");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);

    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    install_reference_transaction_hook(&repo, &log, true);

    let tip = repo.refs().resolve("HEAD").expect("head");
    fs::write(&log, "").expect("clear log");
    let err = commit_ref_update_on_store(
        repo.refs(),
        RefUpdate {
            name: "refs/heads/blocked".into(),
            new_value: Some(RawRef::Direct(tip)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        },
    )
    .expect_err("hook blocks");
    assert!(matches!(err, Error::Hook(_)), "{err}");
    assert!(repo
        .refs()
        .read_raw("refs/heads/blocked")
        .expect("read")
        .is_none());
    let text = read_log(&log);
    assert!(text.contains("prepared:"), "{text}");
    assert!(text.contains("aborted:"), "{text}");
    assert!(!text.contains("committed:"), "{text}");
}

#[test]
fn files_backend_commit_emits_head_and_branch_lines() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("rt.log");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "a@b.c"]);
    git(tmp.path(), &["config", "user.name", "n"]);
    fs::write(tmp.path().join("f"), b"x").expect("file");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);

    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    install_reference_transaction_hook(&repo, &log, false);

    git(tmp.path(), &["commit", "--allow-empty", "-qm", "c2"]);
    let new_commit = repo.refs().resolve("HEAD").expect("new tip");
    git(tmp.path(), &["reset", "--hard", "HEAD~1"]);
    let old = repo.refs().resolve("HEAD").expect("old tip");

    fs::write(&log, "").expect("clear log");
    update_branch_for_commit_on_store(
        repo.refs(),
        &BranchCommitRefUpdate {
            branch_ref: "refs/heads/main",
            expected_old: Some(old),
            new_oid: new_commit,
            identity: "T <t@e.x> 1700000000 +0000",
            reflog_message: "commit: c2",
        },
        &repo.config().expect("config"),
    )
    .expect("advance branch");

    let text = read_log(&log);
    assert_eq!(text.matches("prepared:").count(), 1, "{text}");
    assert_eq!(text.matches("committed:").count(), 1, "{text}");
    assert!(!text.contains("aborted:"), "{text}");
    assert!(text.contains("HEAD|"), "{text}");
    assert!(text.contains("refs/heads/main|"), "{text}");
    assert!(text.contains(&format!("{old} {new_commit} HEAD")), "{text}");
}

#[test]
fn sha256_initial_commit_hook_uses_wide_null_old_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("rt.log");
    git(
        tmp.path(),
        &["init", "-q", "--object-format=sha256", "-b", "main", "."],
    );
    git(tmp.path(), &["config", "user.email", "a@b.c"]);
    git(tmp.path(), &["config", "user.name", "n"]);
    fs::write(tmp.path().join("f"), b"x").expect("file");
    git(tmp.path(), &["add", "f"]);

    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
    install_reference_transaction_hook(&repo, &log, false);

    fs::write(&log, "").expect("clear log");
    stage_for_commit(&repo, &mut NullProgress, &grit_lib::progress::NeverCancel).expect("stage");
    let ident = "T <t@e.x> 1700000000 +0000".to_string();
    create_commit(
        &repo,
        &{
            let mut o = CommitRequest::default();
            o.message = "c1".into();
            o.author = Some(ident.parse().expect("sig"));
            o.committer = Some(ident.parse().expect("sig"));
            o.allow_empty = false;
            o
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    let null = ObjectId::null(HashAlgo::Sha256).to_hex();
    assert_eq!(null.len(), 64, "sha256 null width");
    let text = read_log(&log);
    assert!(
        text.contains(&format!("{null} ")),
        "expected 64-hex null old id in hook stdin, got: {text}"
    );
    assert!(
        text.contains(" HEAD|") || text.contains(" HEAD\n"),
        "{text}"
    );
}

#[test]
fn branch_lock_held_during_prepared_blocks_nested_ref_update() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("rt.log");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "a@b.c"]);
    git(tmp.path(), &["config", "user.name", "n"]);
    fs::write(tmp.path().join("f"), b"x").expect("file");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);

    git(tmp.path(), &["commit", "--allow-empty", "-qm", "c2"]);
    let target = git_output(tmp.path(), &["rev-parse", "HEAD"]);
    git(tmp.path(), &["reset", "--hard", "HEAD~1"]);
    let old = git_output(tmp.path(), &["rev-parse", "HEAD"]);

    let hooks = tmp.path().join(".git/hooks");
    fs::create_dir_all(&hooks).expect("hooks");
    let script = format!(
        "#!/bin/sh\n\
         stdin=$(mktemp)\n\
         cat > \"$stdin\"\n\
         echo \"$1: $(tr '\\n' '|' < \"$stdin\")\" >> {log}\n\
         if [ \"$1\" = prepared ]; then\n\
           read -r o n r < \"$stdin\" || exit 0\n\
           git update-ref \"$r\" \"$o\" 2>/dev/null || true\n\
         fi\n\
         rm -f \"$stdin\"\n\
         exit 0\n",
        log = log.display()
    );
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, script).expect("hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");

    let repo = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");
    let new_oid = target.parse().expect("parse oid");
    let old_oid = old.parse().expect("parse old");

    fs::write(&log, "").expect("clear log");
    update_branch_for_commit_on_store(
        repo.refs(),
        &BranchCommitRefUpdate {
            branch_ref: "refs/heads/main",
            expected_old: Some(old_oid),
            new_oid,
            identity: "T <t@e.x> 1700000000 +0000",
            reflog_message: "commit: c2",
        },
        &repo.config().expect("config"),
    )
    .expect("outer commit succeeds");

    assert_eq!(
        repo.refs().resolve("refs/heads/main").expect("tip"),
        new_oid,
        "branch must stay at grit's new tip, not the hook's rewind"
    );
    let text = read_log(&log);
    assert_eq!(text.matches("prepared:").count(), 1, "{text}");
    assert_eq!(text.matches("committed:").count(), 1, "{text}");
}

fn git_output(repo_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[test]
fn core_hooks_path_is_honored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("mkdir");
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["config", "user.email", "a@b.c"]);
    git(&work, &["config", "user.name", "n"]);
    let hooks = root.join("alt-hooks");
    fs::create_dir_all(&hooks).expect("hooks");
    let log = root.join("rt.log");
    git(&work, &["config", "core.hooksPath", "../alt-hooks"]);
    let script = format!(
        "#!/bin/sh\n echo \"$1 prepared\" >> {}\n exit 0\n",
        log.display()
    );
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, script).expect("hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::write(work.join("f"), b"x").expect("file");
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-qm", "c1"]);

    let repo = Repository::discover(Some(&work), &RepositoryOptions::empty()).expect("open");
    let tip = repo.refs().resolve("HEAD").expect("head");
    fs::write(&log, "").expect("clear log");
    commit_ref_update_on_store(
        repo.refs(),
        RefUpdate {
            name: "refs/heads/x".into(),
            new_value: Some(RawRef::Direct(tip)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        },
    )
    .expect("branch");
    let text = read_log(&log);
    assert!(text.contains("prepared prepared"), "{text}");
    assert!(text.contains("committed prepared"), "{text}");
}

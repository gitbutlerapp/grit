//! The recorded commit message comes from the request, not from the shared `COMMIT_EDITMSG`
//! file, unless a `commit-msg` hook ran and may have edited it (issue #1791).
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;
use grit_lib::HookName;

fn commit_req(message: &str) -> CommitRequest {
    let id = "Msg Source <msg@example.com> 1700000000 +0000".to_owned();
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Msg Source")
        .env("GIT_AUTHOR_EMAIL", "msg@example.com")
        .env("GIT_COMMITTER_NAME", "Msg Source")
        .env("GIT_COMMITTER_EMAIL", "msg@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed_repo(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"1\n").expect("a");
    fs::write(root.join("b.txt"), b"1\n").expect("b");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn install_hook(root: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).expect("hooks dir");
    let path = hooks.join(name);
    fs::write(&path, body).expect("write hook");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

/// Make every write to `COMMIT_EDITMSG` vanish and every read return nothing, which is what a
/// committer sees when another process truncates the shared file between its write and read.
fn clobber_commit_editmsg(root: &Path) {
    let path = root.join(".git/COMMIT_EDITMSG");
    let _ = fs::remove_file(&path);
    std::os::unix::fs::symlink("/dev/null", &path).expect("symlink");
}

fn paths(spec: &str) -> CommitScope {
    CommitScope::Paths(ResolvedPathspecs {
        specs: vec![spec.to_owned()],
        sources: vec![spec.to_owned()],
        scoped: true,
    })
}

fn head_subject(root: &Path) -> String {
    git(root, &["log", "-1", "--format=%s"])
        .trim_end()
        .to_owned()
}

#[test]
fn auto_stage_commit_uses_request_message_when_editmsg_is_clobbered() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = seed_repo(root);
    fs::write(root.join("a.txt"), b"2\n").expect("edit");
    clobber_commit_editmsg(root);

    let outcome = commit_with_scope(
        &repo,
        &commit_req("mine"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    assert!(outcome.hooks_run.is_empty());
    assert_eq!(head_subject(root), "mine");
    git(root, &["fsck", "--strict", "--no-progress"]);
}

#[test]
fn paths_commit_uses_request_message_when_editmsg_is_clobbered() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = seed_repo(root);
    fs::write(root.join("a.txt"), b"2\n").expect("edit a");
    fs::write(root.join("b.txt"), b"2\n").expect("edit b");
    clobber_commit_editmsg(root);

    commit_with_scope(
        &repo,
        &commit_req("msg-for-a"),
        paths("a.txt"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    assert_eq!(head_subject(root), "msg-for-a");
    assert_eq!(
        git(root, &["diff", "--name-only", "HEAD~1", "HEAD"]),
        "a.txt\n"
    );
    git(root, &["fsck", "--strict", "--no-progress"]);
}

#[test]
fn commit_msg_hook_edit_is_recorded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = seed_repo(root);
    install_hook(
        root,
        "commit-msg",
        "#!/bin/sh\nprintf 'rewritten by hook\\n\\nwas: %s\\n' \"$(head -n1 \"$1\")\" > \"$1\"\n",
    );
    fs::write(root.join("a.txt"), b"2\n").expect("edit a");
    fs::write(root.join("b.txt"), b"2\n").expect("edit b");

    let outcome = commit_with_scope(
        &repo,
        &commit_req("original"),
        paths("a.txt"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    assert!(outcome.hooks_run.contains(&HookName::CommitMsg));
    assert_eq!(
        git(root, &["log", "-1", "--format=%B"]),
        "rewritten by hook\n\nwas: original\n\n"
    );
}

#[test]
fn commit_msg_hook_emptying_message_aborts_without_moving_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = seed_repo(root);
    install_hook(root, "commit-msg", "#!/bin/sh\n: > \"$1\"\n");
    fs::write(root.join("a.txt"), b"2\n").expect("edit");
    let before = git(root, &["rev-parse", "HEAD"]);

    let err = commit_with_scope(
        &repo,
        &commit_req("will be emptied"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("emptied message must abort");

    assert!(matches!(err, Error::EmptyCommitMessage), "{err:?}");
    assert_eq!(git(root, &["rev-parse", "HEAD"]), before);
}

#[test]
fn commits_keep_their_message_while_editmsg_is_rewritten_concurrently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = seed_repo(root);

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let path = root.join(".git/COMMIT_EDITMSG");
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::write(&path, "");
                let _ = fs::write(&path, "someone else's message\n");
            }
        })
    };

    let mut failures = Vec::new();
    for i in 0..40 {
        fs::write(root.join("a.txt"), format!("{i}\n")).expect("edit");
        let message = format!("round-{i}");
        match commit_with_scope(
            &repo,
            &commit_req(&message),
            paths("a.txt"),
            &mut NullProgress,
            &grit_lib::progress::NeverCancel,
        ) {
            Ok(_) if head_subject(root) == message => {}
            Ok(_) => failures.push(format!("{message}: recorded {:?}", head_subject(root))),
            Err(err) => failures.push(format!("{message}: {err}")),
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer thread");

    assert!(failures.is_empty(), "{failures:#?}");
    git(root, &["fsck", "--strict", "--no-progress"]);
}

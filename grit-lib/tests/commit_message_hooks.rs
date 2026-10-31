//! `pre-merge-commit`, `prepare-commit-msg`, `commit-msg` and `pre-rebase` around commit, merge
//! and rebase (issue #2201).
//!
//! Each test installs real `.git/hooks` scripts that append their name, arguments and whether
//! `MERGE_HEAD` exists to a log, drives the library, and checks the log line by line. Where the
//! exact argv matters, the same scenario is replayed with the system `git` and the logs compared.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::command_runner::hooks::HookError;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitOutcome, CommitRequest, HookPolicy};
use grit_lib::porcelain::merge::{
    continue_merge, integrate_branch_with, ContinueOptions, IntegrateBranchOutcome,
    MergeCommitMode, MergeOptions,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    plan_rebase_with_hook, start_rebase, ApplyOptions, EmptyPolicy, HistoryError, PreRebase,
    RebaseOntoOptions, RebaseOutcome, RebasePlan, RebaseRequest, RebaseSessionError, RefScope,
    RewriteOptions, StartRebaseOptions,
};
use grit_lib::{HookName, PostOperationHooks};

const IDENT: &str = "Hook Tester <hooks@grit.test> 1700000000 +0000";
const LOGGED_HOOKS: [&str; 6] = [
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-merge",
    "pre-rebase",
];

struct Fixture {
    dir: tempfile::TempDir,
    log: PathBuf,
}

impl Fixture {
    /// Repository on `main` with the logging hooks installed and an empty log.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join(".git").join("hook.log");
        let fixture = Self { dir, log };
        fixture.git(&["init", "-q", "-b", "main", "."]);
        fixture.git(&["config", "user.name", "Hook Tester"]);
        fixture.git(&["config", "user.email", "hooks@grit.test"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        for hook in LOGGED_HOOKS {
            fixture.write_hook(hook, "");
        }
        fixture
    }

    fn wt(&self) -> &Path {
        self.dir.path()
    }

    fn git_dir(&self) -> PathBuf {
        self.wt().join(".git")
    }

    /// Install a hook that logs `<name> <args> mergehead=<yes|no>` and then runs `extra`.
    fn write_hook(&self, name: &str, extra: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.git_dir().join("hooks").join(name);
        std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir hooks");
        let body = format!(
            "#!/bin/sh\n\
             mh=no; [ -f '{git_dir}/MERGE_HEAD' ] && mh=yes\n\
             echo \"{name} $* mergehead=$mh\" >> '{log}'\n\
             {extra}\n",
            git_dir = self.git_dir().display(),
            log = self.log.display(),
        );
        std::fs::write(&path, body).expect("write hook");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Run system git with the hooks disabled, so setup steps never reach the log.
    fn git(&self, args: &[&str]) -> String {
        let out = self.git_command(args, true).output().expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git_command(&self, args: &[&str], hooks_off: bool) -> Command {
        let mut cmd = Command::new("git");
        if hooks_off {
            cmd.args(["-c", "core.hooksPath=/dev/null"]);
        }
        cmd.args(args)
            .current_dir(self.wt())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000");
        cmd
    }

    fn oid(&self, spec: &str) -> ObjectId {
        ObjectId::from_hex(self.git(&["rev-parse", "--verify", spec]).trim()).expect("oid")
    }

    fn commit(&self, path: &str, contents: &str, msg: &str) -> ObjectId {
        std::fs::write(self.wt().join(path), contents).expect("write");
        self.git(&["add", path]);
        self.git(&["commit", "-qm", msg]);
        self.oid("HEAD")
    }

    fn repo(&self) -> Repository {
        let env = Environment::from_vars(std::env::vars_os(), self.wt().to_path_buf());
        let opts = RepositoryOptions::with_environment(env).with_work_tree(self.wt());
        Repository::open(&self.git_dir(), &opts).expect("open repo")
    }

    /// Log lines written since the last call (the log is cleared after reading).
    fn take_log(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let _ = std::fs::remove_file(&self.log);
        text.lines().map(str::to_owned).collect()
    }

    fn message(&self, rev: &str) -> String {
        self.git(&["log", "-1", "--format=%B", rev])
    }

    fn fsck(&self) {
        self.git(&["fsck", "--strict", "--no-progress"]);
    }
}

fn request(message: &str) -> CommitRequest {
    {
        let mut o = CommitRequest::default();
        o.message = message.into();
        o.author = Some(IDENT.parse().expect("signature"));
        o.committer = Some(IDENT.parse().expect("signature"));
        o.allow_empty = true;
        o
    }
}

fn commit(repo: &Repository, req: &CommitRequest) -> grit_lib::error::Result<CommitOutcome> {
    create_commit(repo, req, &mut NullProgress, &NeverCancel)
}

fn hook_failed(err: &Error) -> Option<&str> {
    match err {
        Error::Hook(HookError::Failed { hook_name, .. }) => Some(hook_name.as_str()),
        _ => None,
    }
}

/// `main` with `f` at c1 → c2 and `side` adding `s` on top of c1: a clean non-ff merge.
fn diverged(fx: &Fixture) -> (ObjectId, ObjectId) {
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    let side = fx.commit("s", "s\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    let main = fx.commit("f", "2\n", "c2");
    (main, side)
}

fn merge(
    fx: &Fixture,
    repo: &Repository,
    options: &MergeOptions,
) -> grit_lib::error::Result<IntegrateBranchOutcome> {
    integrate_branch_with(
        repo,
        "refs/heads/main",
        fx.oid("main"),
        fx.oid("side"),
        "side",
        options,
        &mut NullProgress,
        &NeverCancel,
    )
}

fn with_message(message: &str) -> MergeOptions {
    MergeOptions {
        message: Some(message.into()),
        ..MergeOptions::default()
    }
}

#[test]
fn commit_runs_prepare_commit_msg_between_pre_commit_and_commit_msg() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.write_hook("prepare-commit-msg", "echo PREPARED >> \"$1\"");
    let repo = fx.repo();

    let outcome = commit(&repo, &request("normal thing")).expect("commit");
    assert_eq!(
        outcome.hooks_run,
        vec![
            HookName::PreCommit,
            HookName::PrepareCommitMsg,
            HookName::CommitMsg
        ]
    );
    assert_eq!(
        fx.take_log(),
        vec![
            "pre-commit  mergehead=no",
            "prepare-commit-msg .git/COMMIT_EDITMSG message mergehead=no",
            "commit-msg .git/COMMIT_EDITMSG mergehead=no",
        ]
    );
    assert_eq!(fx.message("HEAD"), "normal thing\nPREPARED\n\n");
    fx.fsck();
}

#[test]
fn no_verify_still_runs_prepare_commit_msg() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let repo = fx.repo();

    let req = {
        let mut o = request("skip verify");
        o.hooks = HookPolicy::SkipVerify;
        o
    };
    let outcome = commit(&repo, &req).expect("commit");
    assert_eq!(outcome.hooks_run, vec![HookName::PrepareCommitMsg]);
    assert_eq!(
        fx.take_log(),
        vec!["prepare-commit-msg .git/COMMIT_EDITMSG message mergehead=no"]
    );
}

#[test]
fn reused_and_amended_messages_name_their_source_commit() {
    let fx = Fixture::new();
    let c1 = fx.commit("f", "1\n", "c1");
    let repo = fx.repo();

    let reuse = {
        let mut o = request("");
        o.reuse_message_from = Some(c1);
        o
    };
    let reused = commit(&repo, &reuse).expect("reuse");
    let amend = {
        let mut o = request("");
        o.amend = true;
        o
    };
    commit(&repo, &amend).expect("amend");
    let log = fx.take_log();
    let prepares: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("prepare-commit-msg"))
        .collect();
    assert_eq!(
        prepares,
        vec![
            &format!(
                "prepare-commit-msg .git/COMMIT_EDITMSG commit {} mergehead=no",
                c1.to_hex()
            ),
            &format!(
                "prepare-commit-msg .git/COMMIT_EDITMSG commit {} mergehead=no",
                reused.oid.to_hex()
            ),
        ]
    );
    assert_eq!(fx.message("HEAD"), "c1\n\n");
}

#[test]
fn amend_with_message_reports_message_like_git() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let repo = fx.repo();

    let amend = {
        let mut o = request("reworded");
        o.amend = true;
        o
    };
    commit(&repo, &amend).expect("amend");
    let grit_log = fx.take_log();

    let theirs = Fixture::new();
    theirs.commit("f", "1\n", "c1");
    let out = theirs
        .git_command(&["commit", "-q", "--amend", "-m", "reworded"], false)
        .output()
        .expect("git amend");
    assert!(out.status.success());
    assert_eq!(grit_log, theirs.take_log());
}

#[test]
fn finishing_a_squash_or_merge_names_squash_or_merge() {
    let fx = Fixture::new();
    diverged(&fx);
    let repo = fx.repo();

    let squash = MergeOptions {
        commit: MergeCommitMode::Squash,
        hooks: PostOperationHooks::Skip,
        ..MergeOptions::default()
    };
    assert_eq!(
        merge(&fx, &repo, &squash).expect("squash"),
        IntegrateBranchOutcome::Squashed
    );
    assert!(fx.take_log().is_empty(), "a squash records no commit");
    commit(&repo, &request("")).expect("commit squash");
    assert!(fx
        .take_log()
        .contains(&"prepare-commit-msg .git/COMMIT_EDITMSG squash mergehead=no".to_owned()));

    fx.git(&["reset", "-q", "--hard", "HEAD~1"]);
    let no_commit = MergeOptions {
        commit: MergeCommitMode::NoCommit,
        ..MergeOptions::default()
    };
    assert_eq!(
        merge(&fx, &repo, &no_commit).expect("no-commit"),
        IntegrateBranchOutcome::Stopped
    );
    assert!(fx.take_log().is_empty(), "--no-commit runs no commit hooks");
    commit(&repo, &request("")).expect("conclude merge");
    assert!(fx
        .take_log()
        .contains(&"prepare-commit-msg .git/COMMIT_EDITMSG merge mergehead=yes".to_owned()));

    fx.git(&["reset", "-q", "--hard", "HEAD~1"]);
    merge(&fx, &repo, &no_commit).expect("no-commit");
    continue_merge(
        &repo,
        &ContinueOptions {
            message: None,
            author: None,
            committer: None,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("continue");
    assert!(fx
        .take_log()
        .contains(&"prepare-commit-msg .git/COMMIT_EDITMSG merge mergehead=yes".to_owned()));
    fx.fsck();
}

/// `main` and `side` both change `f`: merging `side` conflicts.
fn conflicting_side(fx: &Fixture) {
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    fx.commit("f", "side\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("f", "main\n", "c2");
}

/// The `prepare-commit-msg` line logged when committing a resolved conflict without a message.
fn prepare_line_after_resolving(fx: &Fixture) -> Option<String> {
    std::fs::write(fx.wt().join("f"), "resolved\n").expect("resolve");
    fx.git(&["add", "f"]);
    fx.take_log();
    commit(&fx.repo(), &request("")).expect("commit resolution");
    fx.take_log()
        .into_iter()
        .find(|l| l.starts_with("prepare-commit-msg"))
}

#[test]
fn committing_a_conflicted_merge_or_squash_names_its_source_like_git() {
    for squash in [false, true] {
        let mode = if squash {
            MergeCommitMode::Squash
        } else {
            MergeCommitMode::Commit
        };
        let fx = Fixture::new();
        conflicting_side(&fx);
        let outcome = merge(
            &fx,
            &fx.repo(),
            &MergeOptions {
                commit: mode,
                ..MergeOptions::default()
            },
        )
        .expect("merge");
        assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
        let grit_line = prepare_line_after_resolving(&fx);

        let theirs = Fixture::new();
        conflicting_side(&theirs);
        let mut args = vec!["merge", "-q"];
        if squash {
            args.push("--squash");
        }
        args.push("side");
        let out = theirs.git_command(&args, true).output().expect("git merge");
        assert!(!out.status.success(), "the merge conflicts");
        std::fs::write(theirs.wt().join("f"), "resolved\n").expect("resolve");
        theirs.git(&["add", "f"]);
        theirs.take_log();
        let out = theirs
            .git_command(&["commit", "-q", "--no-edit"], false)
            .output()
            .expect("git commit");
        assert!(out.status.success());
        let git_line = theirs
            .take_log()
            .into_iter()
            .find(|l| l.starts_with("prepare-commit-msg"));

        let source = if squash { "squash" } else { "merge" };
        assert_eq!(
            git_line.as_deref(),
            Some(
                format!(
                    "prepare-commit-msg .git/COMMIT_EDITMSG {source} mergehead={}",
                    if squash { "no" } else { "yes" }
                )
                .as_str()
            ),
            "system git reference changed"
        );
        assert_eq!(grit_line, git_line, "squash={squash}");
        fx.fsck();
    }
}

#[test]
fn failing_prepare_commit_msg_leaves_head_unchanged() {
    let fx = Fixture::new();
    let c1 = fx.commit("f", "1\n", "c1");
    fx.write_hook("prepare-commit-msg", "exit 7");
    let repo = fx.repo();

    let req = {
        let mut o = request("refused");
        o.hooks = HookPolicy::SkipVerify;
        o
    };
    let err = commit(&repo, &req).expect_err("prepare-commit-msg refuses");
    assert_eq!(hook_failed(&err), Some("prepare-commit-msg"));
    assert_eq!(fx.oid("HEAD"), c1);
}

#[test]
fn clean_merge_runs_message_hooks_with_merge_head_like_git() {
    let fx = Fixture::new();
    let (main, side) = diverged(&fx);
    fx.write_hook("prepare-commit-msg", "echo PREPARED >> \"$1\"");
    let repo = fx.repo();

    let outcome = merge(&fx, &repo, &with_message("merge side")).expect("merge");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected a merge commit, got {outcome:?}");
    };
    let grit_log = fx.take_log();
    assert_eq!(
        grit_log,
        vec![
            "pre-merge-commit  mergehead=no",
            "prepare-commit-msg .git/MERGE_MSG merge mergehead=yes",
            "commit-msg .git/MERGE_MSG mergehead=yes",
            "post-merge 0 mergehead=yes",
        ]
    );
    assert_eq!(fx.oid("main"), oid);
    assert_eq!(fx.oid("main^1"), main);
    assert_eq!(fx.oid("main^2"), side);
    assert!(fx.message("main").starts_with("merge side\nPREPARED\n"));
    for leftover in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        assert!(!fx.git_dir().join(leftover).exists(), "{leftover} left");
    }
    assert_eq!(fx.oid("ORIG_HEAD"), main);
    fx.fsck();

    let theirs = Fixture::new();
    diverged(&theirs);
    let out = theirs
        .git_command(&["merge", "-q", "side", "-m", "merge side"], false)
        .output()
        .expect("git merge");
    assert!(out.status.success());
    assert_eq!(grit_log, theirs.take_log(), "same hooks, argv and order");
}

#[test]
fn refused_pre_merge_commit_leaves_merge_state_like_git() {
    let fx = Fixture::new();
    let (main, side) = diverged(&fx);
    fx.write_hook("pre-merge-commit", "exit 1");
    let repo = fx.repo();

    let err = merge(&fx, &repo, &with_message("WIP merge")).expect_err("refused");
    assert_eq!(hook_failed(&err), Some("pre-merge-commit"));
    let grit_log = fx.take_log();
    assert_eq!(grit_log, vec!["pre-merge-commit  mergehead=no"]);
    assert_eq!(fx.oid("main"), main, "branch must not move");
    assert_eq!(fx.oid("MERGE_HEAD"), side);
    assert_eq!(fx.oid("ORIG_HEAD"), main);
    assert_eq!(
        std::fs::read_to_string(fx.git_dir().join("MERGE_MSG")).expect("MERGE_MSG"),
        "WIP merge\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.wt().join("s")).expect("merged work tree"),
        "s\n"
    );

    let theirs = Fixture::new();
    diverged(&theirs);
    theirs.write_hook("pre-merge-commit", "exit 1");
    let out = theirs
        .git_command(&["merge", "-q", "side", "-m", "WIP merge"], false)
        .output()
        .expect("git merge");
    assert!(!out.status.success());
    assert_eq!(grit_log, theirs.take_log());
    assert_eq!(theirs.oid("MERGE_HEAD"), theirs.oid("side"));
    assert_eq!(theirs.oid("main"), theirs.oid("ORIG_HEAD"));

    fx.git(&["commit", "-q", "--no-edit"]);
    assert_eq!(fx.oid("main^1"), main);
    assert_eq!(fx.oid("main^2"), side);
    fx.fsck();
}

#[test]
fn refused_commit_msg_restores_the_merge_message() {
    let fx = Fixture::new();
    let (main, side) = diverged(&fx);
    fx.write_hook("prepare-commit-msg", "echo PREPARED >> \"$1\"");
    fx.write_hook(
        "commit-msg",
        "grep -q WIP \"$1\" && { echo 'no WIP' >&2; exit 1; }; exit 0",
    );
    let repo = fx.repo();

    let err = merge(&fx, &repo, &with_message("WIP merge")).expect_err("refused");
    assert_eq!(hook_failed(&err), Some("commit-msg"));
    let Error::Hook(HookError::Failed { output, .. }) = &err else {
        unreachable!()
    };
    assert_eq!(output.stderr, b"no WIP\n");
    assert_eq!(fx.oid("main"), main);
    assert_eq!(fx.oid("MERGE_HEAD"), side);
    assert_eq!(
        std::fs::read_to_string(fx.git_dir().join("MERGE_MSG")).expect("MERGE_MSG"),
        "WIP merge\n"
    );
    assert!(
        !fx.take_log().iter().any(|l| l.starts_with("post-merge")),
        "a refused merge runs no post-merge"
    );
}

#[test]
fn changes_staged_by_pre_merge_commit_are_committed() {
    let fx = Fixture::new();
    diverged(&fx);
    fx.write_hook("pre-merge-commit", "echo linted > linted && git add linted");
    let repo = fx.repo();

    merge(&fx, &repo, &MergeOptions::default()).expect("merge");
    assert_eq!(fx.git(&["show", "main:linted"]), "linted\n");
    assert_eq!(fx.git(&["status", "--porcelain"]), "");
    fx.fsck();
}

#[test]
fn merge_no_verify_skips_pre_merge_commit_and_commit_msg_only() {
    let fx = Fixture::new();
    diverged(&fx);
    fx.write_hook("pre-merge-commit", "exit 1");
    fx.write_hook("commit-msg", "exit 1");
    let repo = fx.repo();

    let options = MergeOptions {
        verify: HookPolicy::SkipVerify,
        ..MergeOptions::default()
    };
    let outcome = merge(&fx, &repo, &options).expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));
    assert_eq!(
        fx.take_log(),
        vec![
            "prepare-commit-msg .git/MERGE_MSG merge mergehead=yes",
            "post-merge 0 mergehead=yes",
        ]
    );
}

#[test]
fn fast_forward_and_conflicts_run_no_commit_hooks() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    fx.commit("f", "side\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    let repo = fx.repo();

    let outcome = merge(&fx, &repo, &MergeOptions::default()).expect("ff");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { .. }
    ));
    assert_eq!(fx.take_log(), vec!["post-merge 0 mergehead=no"]);

    fx.git(&["reset", "-q", "--hard", "HEAD~1"]);
    fx.commit("f", "main\n", "c2");
    let outcome = merge(&fx, &repo, &MergeOptions::default()).expect("conflicts");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(fx.take_log().is_empty());
}

fn rebase_opts(upstream: ObjectId, tip: ObjectId, pre_rebase: PreRebase) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: None,
            branch_tip: tip,
        },
        rebase: RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: IDENT.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        }),
        apply: ApplyOptions {
            identity: IDENT.parse().expect("signature"),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: PostOperationHooks::Skip,
        },
        pre_rebase,
    }
}

fn rebase(
    repo: &Repository,
    opts: &StartRebaseOptions,
) -> Result<RebaseOutcome, RebaseSessionError> {
    start_rebase(repo, opts, &mut NullProgress, &NeverCancel)
}

fn history_hook_failed(err: &HistoryError) -> Option<&str> {
    match err {
        HistoryError::Library(lib) => hook_failed(lib),
        _ => None,
    }
}

fn rebase_hook_failed(err: &RebaseSessionError) -> Option<&str> {
    match err {
        RebaseSessionError::History(history) => history_hook_failed(history),
        RebaseSessionError::Library(lib) => hook_failed(lib),
        _ => None,
    }
}

/// `topic` changes `f` while `main` changes it differently: replaying `topic` conflicts.
fn conflicting_topic(fx: &Fixture) -> (ObjectId, ObjectId) {
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "topic"]);
    let topic = fx.commit("f", "topic\n", "t1");
    fx.git(&["switch", "-q", "main"]);
    let main = fx.commit("f", "main\n", "c2");
    fx.git(&["switch", "-q", "topic"]);
    (main, topic)
}

#[test]
fn refused_pre_rebase_wins_over_a_replay_conflict_like_git() {
    let fx = Fixture::new();
    let (main, topic) = conflicting_topic(&fx);
    fx.write_hook("pre-rebase", "exit 1");
    let repo = fx.repo();
    let objects_before = fx.git(&["count-objects"]);

    let hook = PreRebase::Run {
        upstream: Some("main".into()),
    };
    let err = rebase(&repo, &rebase_opts(main, topic, hook.clone())).expect_err("refused");
    assert_eq!(rebase_hook_failed(&err), Some("pre-rebase"), "{err:?}");
    assert!(!fx.git_dir().join("rebase-merge").exists());
    assert_eq!(fx.oid("HEAD"), topic);

    let request = RebaseRequest {
        upstream: main,
        onto: None,
        branch_tip: topic,
    };
    let opts = rebase_opts(main, topic, PreRebase::Skip).rebase;
    let err = plan_rebase_with_hook(&repo, &request, &opts, &hook).expect_err("refused");
    assert_eq!(history_hook_failed(&err), Some("pre-rebase"), "{err:?}");
    assert_eq!(
        fx.git(&["count-objects"]),
        objects_before,
        "a refused rebase writes no objects"
    );
    assert!(matches!(
        plan_rebase_with_hook(&repo, &request, &opts, &PreRebase::Skip).expect("plan"),
        RebasePlan::Conflict { .. }
    ));
    assert_eq!(
        fx.take_log(),
        vec![
            "pre-rebase main mergehead=no",
            "pre-rebase main mergehead=no"
        ]
    );

    let theirs = Fixture::new();
    conflicting_topic(&theirs);
    theirs.write_hook("pre-rebase", "exit 1");
    let out = theirs
        .git_command(&["rebase", "-q", "main"], false)
        .output()
        .expect("git rebase");
    assert!(!out.status.success());
    assert_eq!(theirs.take_log(), vec!["pre-rebase main mergehead=no"]);
    assert!(!theirs.git_dir().join("rebase-merge").exists());
}

/// `topic` (one commit) forked from `main`, which moved on; `topic` is checked out and tracks
/// `main` through the local remote.
fn topic_behind_main(fx: &Fixture) -> (ObjectId, ObjectId) {
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "topic"]);
    let topic = fx.commit("t", "t\n", "t1");
    fx.git(&["switch", "-q", "main"]);
    let main = fx.commit("f", "2\n", "c2");
    fx.git(&["switch", "-q", "topic"]);
    fx.git(&["config", "branch.topic.remote", "."]);
    fx.git(&["config", "branch.topic.merge", "refs/heads/main"]);
    (main, topic)
}

#[test]
fn rebase_runs_pre_rebase_with_the_upstream_as_named() {
    let fx = Fixture::new();
    let (main, topic) = topic_behind_main(&fx);
    let repo = fx.repo();

    let opts = rebase_opts(
        main,
        topic,
        PreRebase::Run {
            upstream: Some("main".into()),
        },
    );
    assert!(matches!(
        rebase(&repo, &opts).expect("rebase"),
        RebaseOutcome::Done { .. }
    ));
    let grit_log = fx.take_log();
    assert_eq!(grit_log, vec!["pre-rebase main mergehead=no"]);
    assert_eq!(fx.oid("topic~1"), main);

    let theirs = Fixture::new();
    topic_behind_main(&theirs);
    let out = theirs
        .git_command(&["rebase", "-q", "main"], false)
        .output()
        .expect("git rebase");
    assert!(out.status.success());
    let git_log: Vec<String> = theirs
        .take_log()
        .into_iter()
        .filter(|l| l.starts_with("pre-rebase"))
        .collect();
    assert_eq!(grit_log, git_log);
}

#[test]
fn default_upstream_reaches_pre_rebase_as_its_full_ref_like_git() {
    let fx = Fixture::new();
    let (main, topic) = topic_behind_main(&fx);
    let repo = fx.repo();

    rebase(
        &repo,
        &rebase_opts(main, topic, PreRebase::Run { upstream: None }),
    )
    .expect("rebase");
    let grit_log = fx.take_log();
    assert_eq!(grit_log, vec!["pre-rebase refs/heads/main mergehead=no"]);

    let theirs = Fixture::new();
    topic_behind_main(&theirs);
    let out = theirs
        .git_command(&["rebase", "-q"], false)
        .output()
        .expect("git rebase");
    assert!(out.status.success());
    let git_log: Vec<String> = theirs
        .take_log()
        .into_iter()
        .filter(|l| l.starts_with("pre-rebase"))
        .collect();
    assert_eq!(grit_log, git_log);
}

#[test]
fn refused_pre_rebase_changes_nothing() {
    let fx = Fixture::new();
    let (main, topic) = topic_behind_main(&fx);
    fx.write_hook("pre-rebase", "exit 1");
    let repo = fx.repo();

    let opts = rebase_opts(
        main,
        topic,
        PreRebase::Run {
            upstream: Some("main".into()),
        },
    );
    let err = rebase(&repo, &opts).expect_err("refused");
    assert_eq!(rebase_hook_failed(&err), Some("pre-rebase"), "{err:?}");
    assert_eq!(fx.oid("HEAD"), topic);
    assert_eq!(fx.oid("topic"), topic);
    assert!(!fx.git_dir().join("rebase-merge").exists());
    assert_eq!(fx.git(&["status", "--porcelain"]), "");

    assert!(matches!(
        rebase(&repo, &rebase_opts(main, topic, PreRebase::Skip)).expect("skip"),
        RebaseOutcome::Done { .. }
    ));
    assert_eq!(fx.oid("topic~1"), main);
}

#[test]
fn up_to_date_rebase_runs_no_pre_rebase() {
    let fx = Fixture::new();
    let (main, _) = topic_behind_main(&fx);
    fx.git(&["reset", "-q", "--hard", "main"]);
    fx.take_log();
    let repo = fx.repo();

    let opts = rebase_opts(
        main,
        main,
        PreRebase::Run {
            upstream: Some("main".into()),
        },
    );
    rebase(&repo, &opts).expect("up to date");
    assert!(fx.take_log().is_empty());
}

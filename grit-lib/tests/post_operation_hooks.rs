//! `post-checkout`, `post-merge` and `post-rewrite` after switch, merge, amend and history
//! rewrites (issue #1816).
//!
//! Each test installs real `.git/hooks` scripts that append their arguments (and, for
//! `post-rewrite`, their stdin) to a log, drives the library, and checks the log line by line
//! with the exact object ids Git would pass.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{
    integrate_branch_with, IntegrateBranchOutcome, MergeCommitMode, MergeOptions,
};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, continue_rebase, drop as history_drop, reword_with, start_rebase,
    undo_last_rewrite, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseOutcome, RebaseRequest,
    RefScope, RewordOptions, RewriteOptions, StartRebaseOptions,
};
use grit_lib::{HookName, PostOperationHooks};

const IDENT: &str = "Hook Tester <hooks@grit.test> 1700000000 +0000";
const LOGGED_HOOKS: [&str; 4] = ["post-checkout", "post-merge", "post-rewrite", "post-commit"];

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
            fixture.write_hook(
                hook,
                &format!(
                    "#!/bin/sh\necho \"{hook} $*\" >> '{log}'\n\
                     if [ {hook} = post-rewrite ]; then\n\
                     while read -r line; do echo \"  $line\" >> '{log}'; done\nfi\n",
                    log = fixture.log.display()
                ),
            );
        }
        fixture
    }

    fn wt(&self) -> &Path {
        self.dir.path()
    }

    fn write_hook(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.wt().join(".git").join("hooks").join(name);
        std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir hooks");
        std::fs::write(&path, body).expect("write hook");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Run system git with the hooks disabled, so setup steps never reach the log.
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(self.wt())
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
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
        Repository::open(&self.wt().join(".git"), &opts).expect("open repo")
    }

    /// Log lines written since the last call (the log is cleared after reading).
    fn take_log(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let _ = std::fs::remove_file(&self.log);
        text.lines().map(str::to_owned).collect()
    }
}

fn checkout_line(old: ObjectId, new: ObjectId) -> String {
    format!("post-checkout {} {} 1", old.to_hex(), new.to_hex())
}

fn pair_line(old: ObjectId, new: ObjectId) -> String {
    format!("  {} {}", old.to_hex(), new.to_hex())
}

fn switch(repo: &Repository, mode: SwitchMode) -> grit_lib::porcelain::switch::SwitchOutcome {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: PostOperationHooks::Run,
        },
    )
    .expect("switch")
}

fn existing(branch: &str, oid: ObjectId) -> SwitchMode {
    SwitchMode::Existing {
        branch: branch.into(),
        oid,
    }
}

/// `main`: c1 → c2, with `side` still at c1.
fn two_commits_and_side(fx: &Fixture) -> (ObjectId, ObjectId) {
    let c1 = fx.commit("f", "1\n", "c1");
    fx.git(&["branch", "side"]);
    let c2 = fx.commit("f", "2\n", "c2");
    (c1, c2)
}

#[test]
fn switch_runs_post_checkout_with_old_and_new_head() {
    let fx = Fixture::new();
    let (c1, c2) = two_commits_and_side(&fx);
    let repo = fx.repo();

    let outcome = switch(&repo, existing("side", c1));
    assert_eq!(outcome.hooks_run, vec![HookName::PostCheckout]);
    assert_eq!(fx.take_log(), vec![checkout_line(c2, c1)]);

    let outcome = switch(
        &repo,
        SwitchMode::Create {
            branch: "nb".into(),
            start_oid: c2,
            create_source_label: "main".into(),
            track_upstream: None,
        },
    );
    assert!(outcome.created);
    assert_eq!(outcome.hooks_run, vec![HookName::PostCheckout]);
    assert_eq!(fx.take_log(), vec![checkout_line(c1, c2)]);

    let outcome = switch(
        &repo,
        SwitchMode::Detached {
            commit: c1,
            reflog_to_label: "side".into(),
        },
    );
    assert!(outcome.detached);
    assert_eq!(fx.take_log(), vec![checkout_line(c2, c1)]);

    switch(&repo, existing("main", c2));
    assert_eq!(fx.take_log(), vec![checkout_line(c1, c2)]);
}

#[test]
fn switch_to_current_branch_or_commit_still_runs_post_checkout() {
    let fx = Fixture::new();
    let (c1, c2) = two_commits_and_side(&fx);
    let repo = fx.repo();

    switch(&repo, existing("main", c2));
    assert_eq!(fx.take_log(), vec![checkout_line(c2, c2)]);

    fx.git(&["switch", "-q", "--detach", &c1.to_hex()]);
    switch(
        &repo,
        SwitchMode::Detached {
            commit: c1,
            reflog_to_label: "HEAD".into(),
        },
    );
    assert_eq!(fx.take_log(), vec![checkout_line(c1, c1)]);
}

#[test]
fn switch_from_unborn_branch_passes_null_old_id() {
    let fx = Fixture::new();
    let (_, c2) = two_commits_and_side(&fx);
    fx.git(&["symbolic-ref", "HEAD", "refs/heads/unborn"]);
    let repo = fx.repo();

    switch(&repo, existing("main", c2));
    assert_eq!(
        fx.take_log(),
        vec![format!(
            "post-checkout {} {} 1",
            "0".repeat(40),
            c2.to_hex()
        )]
    );
}

#[test]
fn switch_dry_run_or_skip_runs_no_hook() {
    let fx = Fixture::new();
    let (c1, _) = two_commits_and_side(&fx);
    let repo = fx.repo();

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: existing("side", c1),
            dry_run: true,
            hooks: PostOperationHooks::Run,
        },
    )
    .expect("dry run");
    assert!(outcome.hooks_run.is_empty());

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: existing("side", c1),
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
    )
    .expect("switch without hooks");
    assert!(outcome.hooks_run.is_empty());
    assert_eq!(fx.oid("HEAD"), c1);
    assert!(fx.take_log().is_empty());
}

#[test]
fn failing_post_checkout_does_not_fail_the_switch() {
    let fx = Fixture::new();
    let (c1, _) = two_commits_and_side(&fx);
    fx.write_hook("post-checkout", "#!/bin/sh\nexit 3\n");
    let repo = fx.repo();

    let outcome = switch(&repo, existing("side", c1));
    assert_eq!(outcome.hooks_run, vec![HookName::PostCheckout]);
    assert_eq!(fx.oid("HEAD"), c1);
}

fn merge(
    fx: &Fixture,
    repo: &Repository,
    other: &str,
    options: &MergeOptions,
) -> IntegrateBranchOutcome {
    integrate_branch_with(
        repo,
        "refs/heads/main",
        fx.oid("main"),
        fx.oid(other),
        other,
        options,
    )
    .expect("merge")
}

#[test]
fn fast_forward_and_merge_commit_run_post_merge_zero() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    fx.commit("s", "s\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    let repo = fx.repo();

    let outcome = merge(&fx, &repo, "side", &MergeOptions::default());
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { .. }
    ));
    assert_eq!(fx.take_log(), vec!["post-merge 0"]);

    fx.git(&["switch", "-q", "side"]);
    fx.commit("s", "s2\n", "s2");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("f", "2\n", "c2");
    let outcome = merge(&fx, &repo, "side", &MergeOptions::default());
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));
    assert_eq!(fx.take_log(), vec!["post-merge 0"]);

    let outcome = merge(&fx, &repo, "side", &MergeOptions::default());
    assert_eq!(outcome, IntegrateBranchOutcome::UpToDate);
    assert!(fx.take_log().is_empty(), "already up to date runs no hook");
}

#[test]
fn squash_merge_runs_post_merge_one() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    fx.commit("s", "s\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("f", "2\n", "c2");
    let repo = fx.repo();

    let outcome = merge(
        &fx,
        &repo,
        "side",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    );
    assert_eq!(outcome, IntegrateBranchOutcome::Squashed);
    assert_eq!(fx.take_log(), vec!["post-merge 1"]);
}

#[test]
fn conflicted_no_commit_or_skipped_merge_runs_no_post_merge() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "side"]);
    fx.commit("s", "s\n", "s1");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("f", "2\n", "c2");
    let repo = fx.repo();

    let outcome = merge(
        &fx,
        &repo,
        "side",
        &MergeOptions {
            hooks: PostOperationHooks::Skip,
            ..MergeOptions::default()
        },
    );
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));
    assert!(fx.take_log().is_empty());

    fx.git(&["switch", "-q", "side"]);
    fx.commit("s", "s2\n", "s2");
    fx.git(&["switch", "-q", "main"]);
    let outcome = merge(
        &fx,
        &repo,
        "side",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            ..MergeOptions::default()
        },
    );
    assert_eq!(outcome, IntegrateBranchOutcome::Stopped);
    assert!(fx.take_log().is_empty());
    fx.git(&["merge", "--abort"]);

    fx.git(&["switch", "-q", "side"]);
    fx.commit("f", "side\n", "s3");
    fx.git(&["switch", "-q", "main"]);
    let outcome = merge(&fx, &repo, "side", &MergeOptions::default());
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(fx.take_log().is_empty());
}

fn commit_request(message: &str, amend: bool) -> CommitRequest {
    CommitRequest {
        message: message.into(),
        author: IDENT.into(),
        committer: IDENT.into(),
        amend,
        allow_empty: true,
        ..Default::default()
    }
}

#[test]
fn amend_runs_post_rewrite_amend_after_post_commit() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let old = fx.commit("f", "2\n", "c2");
    let repo = fx.repo();

    let outcome =
        create_commit(&repo, &commit_request("amended", true), &mut NullProgress).expect("amend");
    assert_eq!(
        outcome.hooks_run,
        vec![HookName::PostCommit, HookName::PostRewrite]
    );
    assert_eq!(
        fx.take_log(),
        vec![
            "post-commit ".to_owned(),
            "post-rewrite amend".to_owned(),
            pair_line(old, outcome.oid),
        ]
    );

    let outcome =
        create_commit(&repo, &commit_request("next", false), &mut NullProgress).expect("commit");
    assert_eq!(outcome.hooks_run, vec![HookName::PostCommit]);
    assert_eq!(fx.take_log(), vec!["post-commit "]);
}

fn rewrite_options() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: IDENT.into(),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_options(record: Option<&str>) -> ApplyOptions {
    ApplyOptions {
        identity: IDENT.into(),
        reflog_message: "history".into(),
        record_operation: record.map(str::to_owned),
        hooks: PostOperationHooks::Run,
    }
}

fn rebase_onto(upstream: ObjectId, tip: ObjectId) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: None,
            branch_tip: tip,
        },
        rebase: RebaseOntoOptions::new(rewrite_options()),
        apply: apply_options(None),
    }
}

#[test]
fn rebase_runs_post_checkout_then_post_rewrite_with_pairs_in_order() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "topic"]);
    let t1 = fx.commit("t1", "t1\n", "t1");
    let t2 = fx.commit("t2", "t2\n", "t2");
    fx.git(&["switch", "-q", "main"]);
    let main = fx.commit("f", "2\n", "c2");
    fx.git(&["switch", "-q", "topic"]);
    let repo = fx.repo();

    let outcome = start_rebase(&repo, &rebase_onto(main, t2)).expect("rebase");
    let RebaseOutcome::Done { applied, .. } = outcome else {
        panic!("expected a clean rebase, got {outcome:?}");
    };
    assert_eq!(
        applied.hooks_run,
        vec![HookName::PostCheckout, HookName::PostRewrite]
    );
    let (new_t1, new_t2) = (fx.oid("topic~1"), fx.oid("topic"));
    assert_eq!(fx.oid("topic~2"), main);
    assert_eq!(
        fx.take_log(),
        vec![
            checkout_line(t2, new_t2),
            "post-rewrite rebase".to_owned(),
            pair_line(t1, new_t1),
            pair_line(t2, new_t2),
        ]
    );
}

#[test]
fn fast_forward_rebase_runs_post_checkout_but_no_post_rewrite() {
    let fx = Fixture::new();
    let (c1, c2) = two_commits_and_side(&fx);
    fx.git(&["switch", "-q", "side"]);
    let repo = fx.repo();

    let outcome = start_rebase(&repo, &rebase_onto(c2, c1)).expect("rebase");
    let RebaseOutcome::Done { applied, .. } = outcome else {
        panic!("expected a fast-forward, got {outcome:?}");
    };
    assert_eq!(applied.hooks_run, vec![HookName::PostCheckout]);
    assert_eq!(fx.oid("side"), c2);
    assert_eq!(fx.take_log(), vec![checkout_line(c1, c2)]);
}

#[test]
fn conflicted_rebase_runs_post_checkout_on_stop_and_post_rewrite_on_continue() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    fx.git(&["switch", "-q", "-c", "topic"]);
    let t1 = fx.commit("f", "topic\n", "t1");
    let t2 = fx.commit("t2", "t2\n", "t2");
    fx.git(&["switch", "-q", "main"]);
    let main = fx.commit("f", "main\n", "c2");
    fx.git(&["switch", "-q", "topic"]);
    let repo = fx.repo();
    let opts = rebase_onto(main, t2);

    let outcome = start_rebase(&repo, &opts).expect("start");
    assert!(matches!(outcome, RebaseOutcome::Stopped(_)), "{outcome:?}");
    assert_eq!(fx.take_log(), vec![checkout_line(t2, main)]);

    std::fs::write(fx.wt().join("f"), "resolved\n").expect("resolve");
    fx.git(&["add", "f"]);
    let outcome = continue_rebase(&repo, &opts).expect("continue");
    let RebaseOutcome::Done { applied, .. } = outcome else {
        panic!("expected the rebase to finish, got {outcome:?}");
    };
    assert_eq!(applied.hooks_run, vec![HookName::PostRewrite]);
    let (new_t1, new_t2) = (fx.oid("topic~1"), fx.oid("topic"));
    assert_eq!(
        fx.take_log(),
        vec![
            "post-rewrite rebase".to_owned(),
            pair_line(t1, new_t1),
            pair_line(t2, new_t2),
        ]
    );
}

#[test]
fn reword_runs_post_rewrite_without_post_checkout() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let c2 = fx.commit("f", "2\n", "c2");
    let c3 = fx.commit("f", "3\n", "c3");
    let repo = fx.repo();

    let plan = reword_with(
        &repo,
        c2,
        &RewordOptions {
            message: Some("c2 reworded\n".into()),
            author: None,
        },
        &rewrite_options(),
    )
    .expect("plan reword");
    let applied = apply_rewrite(&repo, &plan, &apply_options(None)).expect("apply");
    assert_eq!(applied.hooks_run, vec![HookName::PostRewrite]);
    assert_eq!(
        fx.take_log(),
        vec![
            "post-rewrite rebase".to_owned(),
            pair_line(c2, fx.oid("main~1")),
            pair_line(c3, fx.oid("main")),
        ]
    );
}

#[test]
fn drop_leaves_dropped_commit_out_of_post_rewrite() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let c2 = fx.commit("g", "2\n", "c2");
    let c3 = fx.commit("h", "3\n", "c3");
    let repo = fx.repo();

    let plan = history_drop(&repo, c2, &rewrite_options()).expect("plan drop");
    let applied = apply_rewrite(&repo, &plan, &apply_options(None)).expect("apply");
    let new_c3 = fx.oid("main");
    assert_eq!(
        applied.hooks_run,
        vec![HookName::PostCheckout, HookName::PostRewrite]
    );
    assert_eq!(
        fx.take_log(),
        vec![
            checkout_line(c3, new_c3),
            "post-rewrite rebase".to_owned(),
            pair_line(c3, new_c3),
        ]
    );
}

#[test]
fn skipped_rewrite_hooks_and_undo_run_nothing() {
    let fx = Fixture::new();
    fx.commit("f", "1\n", "c1");
    let c2 = fx.commit("g", "2\n", "c2");
    let repo = fx.repo();

    let plan = history_drop(&repo, c2, &rewrite_options()).expect("plan drop");
    let applied = apply_rewrite(
        &repo,
        &plan,
        &ApplyOptions {
            hooks: PostOperationHooks::Skip,
            ..apply_options(Some("drop"))
        },
    )
    .expect("apply");
    assert!(applied.hooks_run.is_empty());
    assert!(fx.take_log().is_empty());

    let undone = undo_last_rewrite(&repo, &apply_options(None), false).expect("undo");
    assert_eq!(undone.head, c2);
    assert!(fx.take_log().is_empty(), "undo runs no hooks");
}

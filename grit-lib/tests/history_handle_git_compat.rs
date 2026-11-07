//! `repo.history()` round-trips with the system `git`: fixups and rebases planned and applied
//! through the handle leave a repository `git fsck --strict` accepts, with `change-id` headers
//! carried across every rewrite, and the handle's option-driven forms (undo mode, exec, the
//! `pre-rebase` setting) behave as documented.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::change_id::ChangeId;
use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{hunk_ids, DiffSide};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::hunk_select::{unstaged_file_diffs, HunkSelection};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    ApplyOptions, EmptyPolicy, ExecSpec, FixupOptions, HistoryError, PreRebase, RebaseCheckPlan,
    RebaseOntoOptions, RebaseOutcome, RebasePlan, RebaseRequest, RefScope, RewordOptions,
    RewriteOptions, StartRebaseOptions, UndoMode,
};

use support::{assert_git_fsck_strict, commit_file, git, git_rev_parse, HistoryRepo};

const IDENT: &str = "Rewriter <r@grit.test> 1700000100 +0000";

fn open(wt: &Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: IDENT.parse().expect("committer"),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_opts(operation: &str) -> ApplyOptions {
    ApplyOptions {
        identity: IDENT.parse().expect("identity"),
        reflog_message: format!("history {operation}"),
        record_operation: Some(operation.into()),
        hooks: PostOperationHooks::Skip,
    }
}

fn rev(wt: &Path, spec: &str) -> ObjectId {
    git_rev_parse(wt, spec).unwrap_or_else(|| panic!("rev-parse {spec}"))
}

/// The `change-id` header value `git cat-file commit` shows for `oid`.
fn git_change_id(wt: &Path, oid: ObjectId) -> String {
    let raw = git(wt, &["cat-file", "commit", &oid.to_hex()]);
    raw.lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix("change-id "))
        .unwrap_or_else(|| panic!("no change-id header on {oid}:\n{raw}"))
        .to_owned()
}

fn derived(oid: ObjectId) -> String {
    ChangeId::derived_from_commit_id(&oid).as_str().to_owned()
}

#[test]
fn fixup_through_the_handle_round_trips_with_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "a\n", "a");
    let target = commit_file(wt, "b.txt", "b\n", "b");
    let tip = commit_file(wt, "c.txt", "c\n", "c");
    fs::write(wt.join("b.txt"), "b fixed\n").expect("write");
    git(wt, &["add", "b.txt"]);

    let grit = open(wt);
    let history = grit.history();
    assert!(std::ptr::eq(history.repository(), &grit));
    let staged = history.staged_paths().expect("staged");
    assert_eq!(staged, vec!["b.txt".to_owned()]);
    let plan = history
        .fixup(
            target,
            &FixupOptions {
                message: None,
                empty: EmptyPolicy::Drop,
                rewrite: rewrite_opts(),
                selection: None,
                pathspecs: Vec::new(),
            },
        )
        .expect("plan");
    history.verify_plan(&plan).expect("verify");
    history
        .apply_fixup(&plan, &apply_opts("fixup"), &staged, &CancelToken::never())
        .expect("apply");

    assert_git_fsck_strict(wt);
    let new_target = plan.replacement(target).expect("target rewritten");
    let new_tip = plan.replacement(tip).expect("tip replayed");
    assert_eq!(rev(wt, "HEAD"), new_tip);
    assert_eq!(rev(wt, "HEAD~1"), new_target);
    assert_eq!(git(wt, &["show", "HEAD~1:b.txt"]), "b fixed\n");
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert_eq!(git_change_id(wt, new_target), derived(target));
    assert_eq!(git_change_id(wt, new_tip), derived(tip));

    // A second rewrite keeps the ids the first one stored.
    let plan = history
        .reword(
            new_target,
            &RewordOptions::default().with_message("b, fixed\n"),
            &rewrite_opts(),
        )
        .expect("reword");
    history
        .apply(
            &plan,
            &apply_opts("reword"),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("apply reword");
    assert_git_fsck_strict(wt);
    assert_eq!(git_change_id(wt, rev(wt, "HEAD~1")), derived(target));
    assert_eq!(git_change_id(wt, rev(wt, "HEAD")), derived(tip));
    assert_eq!(
        git(wt, &["log", "-1", "--format=%s", "HEAD~1"]),
        "b, fixed\n"
    );
}

/// `main` gets `base`; `topic` adds `t1` and `t2`; `main` then advances by `m1`.
fn diverged(wt: &Path) -> (ObjectId, ObjectId, ObjectId) {
    commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["switch", "-q", "-c", "topic"]);
    let t1 = commit_file(wt, "t1.txt", "t1\n", "t1");
    let t2 = commit_file(wt, "t2.txt", "t2\n", "t2");
    git(wt, &["switch", "-q", "main"]);
    commit_file(wt, "m1.txt", "m1\n", "m1");
    git(wt, &["switch", "-q", "topic"]);
    (rev(wt, "main"), t1, t2)
}

fn start_opts(repo: &Repository, upstream: ObjectId) -> StartRebaseOptions {
    let branch_tip = repo.head_commit().expect("head");
    let request = repo
        .history()
        .rebase_run_request(branch_tip, upstream, None, Some("main"), false, None)
        .expect("request");
    StartRebaseOptions {
        request,
        rebase: RebaseOntoOptions::new(rewrite_opts()),
        apply: apply_opts("rebase"),
        exec: None,
    }
}

#[test]
fn rebase_through_the_handle_round_trips_with_git_and_keeps_change_ids() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main, t1, t2) = diverged(wt);

    let grit = open(wt);
    let history = grit.history();
    let opts = start_opts(&grit, main);
    assert_eq!(opts.request.upstream, main);
    let RebaseCheckPlan::Clean { commits } = history
        .plan_rebase_check(&opts.request, &opts.rebase)
        .expect("check")
    else {
        panic!("expected a clean rebase");
    };
    assert_eq!(commits.len(), 2);
    let outcome = history
        .start_rebase(
            &opts,
            grit.command_runner().as_ref(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");

    assert_git_fsck_strict(wt);
    assert_eq!(rev(wt, "topic~2"), main);
    assert_eq!(git_change_id(wt, rev(wt, "topic~1")), derived(t1));
    assert_eq!(git_change_id(wt, rev(wt, "topic")), derived(t2));

    // `main` moves again with git; rebasing the already rebased commits keeps their ids.
    git(wt, &["switch", "-q", "main"]);
    let main2 = commit_file(wt, "m2.txt", "m2\n", "m2");
    git(wt, &["switch", "-q", "topic"]);
    let grit = open(wt);
    let opts = start_opts(&grit, main2);
    let outcome = grit
        .history()
        .start_rebase(
            &opts,
            grit.command_runner().as_ref(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("second rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");
    assert_git_fsck_strict(wt);
    assert_eq!(rev(wt, "topic~2"), main2);
    assert_eq!(git_change_id(wt, rev(wt, "topic~1")), derived(t1));
    assert_eq!(git_change_id(wt, rev(wt, "topic")), derived(t2));
    assert_eq!(git(wt, &["log", "--format=%s", "main..topic"]), "t2\nt1\n");
}

#[test]
fn start_rebase_runs_the_exec_option_through_the_given_runner() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main, _, _) = diverged(wt);
    let grit = open(wt);
    let mut opts = start_opts(&grit, main);
    opts.exec = Some(ExecSpec {
        commands: vec!["true".into()],
    });
    let outcome = grit
        .history()
        .start_rebase(
            &opts,
            grit.command_runner().as_ref(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rebase --exec");
    let RebaseOutcome::ExecDone { exec, .. } = outcome else {
        panic!("expected an exec rebase, got {outcome:?}");
    };
    assert_eq!(exec.runs.len(), 2);
    assert_eq!(rev(wt, "topic~2"), main);
    assert_git_fsck_strict(wt);
}

#[test]
fn plan_rebase_runs_pre_rebase_from_the_options_and_check_does_not() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main, _, topic) = diverged(wt);
    let hook = wt.join(".git/hooks/pre-rebase");
    fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
    let log = wt.join("pre-rebase.log");
    fs::write(
        &hook,
        format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", log.display()),
    )
    .expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let grit = open(wt);
    let history = grit.history();
    let request = RebaseRequest {
        upstream: main,
        onto: None,
        branch_tip: topic,
    };
    let mut opts = RebaseOntoOptions::new(rewrite_opts());
    assert_eq!(opts.pre_rebase, PreRebase::Skip);
    opts.pre_rebase = PreRebase::Run {
        upstream: Some("main".into()),
    };

    assert!(matches!(
        history.plan_rebase_check(&request, &opts).expect("check"),
        RebaseCheckPlan::Clean { .. }
    ));
    assert!(!log.exists(), "check ran pre-rebase");

    let err = history
        .plan_rebase(&request, &opts)
        .expect_err("pre-rebase refuses");
    assert!(matches!(&err, grit_lib::error::Error::Hook(_)), "{err:?}");
    assert_eq!(fs::read_to_string(&log).expect("log"), "ran\n");

    opts.pre_rebase = PreRebase::Skip;
    assert!(matches!(
        history.plan_rebase(&request, &opts).expect("plan"),
        RebasePlan::Plan(_)
    ));
    assert_eq!(rev(wt, "topic"), topic, "planning moves no ref");
}

#[test]
fn undo_last_dry_run_reports_and_apply_restores() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "a\n", "a");
    let tip = commit_file(wt, "b.txt", "b\n", "b");
    let grit = open(wt);
    let history = grit.history();
    assert!(history.last().expect("read").is_none());
    let plan = history
        .reword(
            tip,
            &RewordOptions::default().with_message("b reworded\n"),
            &rewrite_opts(),
        )
        .expect("reword");
    history
        .apply(
            &plan,
            &apply_opts("reword"),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    let reworded = rev(wt, "HEAD");
    assert_eq!(
        history.last().expect("read").expect("record").operation,
        "reword"
    );

    let preview = history
        .undo_last(&apply_opts("undo"), UndoMode::DryRun, &CancelToken::never())
        .expect("dry run");
    assert!(preview.dry_run);
    assert_eq!(preview.head, tip);
    assert_eq!(rev(wt, "HEAD"), reworded, "a dry run moves nothing");

    let undone = history
        .undo_last(&apply_opts("undo"), UndoMode::Apply, &CancelToken::never())
        .expect("undo");
    assert!(!undone.dry_run);
    assert_eq!(undone.head_change, Some((reworded, tip)));
    assert_eq!(rev(wt, "HEAD"), tip);
    assert_git_fsck_strict(wt);
}

#[test]
fn journal_absorb_base_and_fetch_request_helpers_on_the_handle() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (main, _, topic) = diverged(wt);
    let grit = open(wt);
    let history = grit.history();

    assert!(!history.apply_journal_active());
    history.ensure_apply_ready().expect("no journal");
    assert_eq!(history.recover_interrupted_apply().expect("recover"), None);
    assert!(matches!(
        history.abort_interrupted_apply(),
        Err(grit_lib::error::Error::History(
            HistoryError::RewriteApplyIncomplete
        ))
    ));

    let base = history.default_absorb_base(topic).expect("absorb base");
    assert_ne!(base, topic);
    let fork = git(wt, &["merge-base", "main", "topic"]);
    for (ancestor, descendant) in [
        (fork.trim(), base.to_hex()),
        (&base.to_hex(), topic.to_hex()),
    ] {
        let out = std::process::Command::new("git")
            .current_dir(wt)
            .args(["merge-base", "--is-ancestor", ancestor, &descendant])
            .status()
            .expect("git merge-base");
        assert!(
            out.success(),
            "{ancestor} is not an ancestor of {descendant}"
        );
    }

    let request = history
        .rebase_request_after_fetch(topic, main, "refs/heads/main", None, false)
        .expect("request");
    assert_eq!(
        request,
        RebaseRequest {
            upstream: main,
            onto: None,
            branch_tip: topic,
        }
    );
}

#[test]
fn fixup_hunks_through_the_handle_fold_only_the_selected_hunk() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    let target = commit_file(wt, "f.txt", &lines, "f");
    commit_file(wt, "g.txt", "g\n", "g");
    let edited = lines
        .replace("line 2\n", "line 2 edited\n")
        .replace("line 19\n", "line 19 edited\n");
    fs::write(wt.join("f.txt"), &edited).expect("write");

    let grit = open(wt);
    let history = grit.history();
    let files = unstaged_file_diffs(&grit, &[]).expect("diff");
    let (file, peer) = &files[0];
    let ids = hunk_ids(DiffSide::Unstaged, &file.path, &file.hunks, peer);
    assert_eq!(ids.len(), 2, "two separate hunks");
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    assert_eq!(
        history
            .fixup_selection_paths(&selection, &[])
            .expect("paths"),
        vec!["f.txt".to_owned()]
    );
    let plan = history
        .fixup(
            target,
            &FixupOptions {
                message: None,
                empty: EmptyPolicy::Drop,
                rewrite: rewrite_opts(),
                selection: Some(selection.clone()),
                pathspecs: Vec::new(),
            },
        )
        .expect("plan");
    history
        .apply_fixup_hunks(
            &plan,
            &apply_opts("fixup"),
            &selection,
            &[],
            &CancelToken::never(),
        )
        .expect("apply");

    assert_git_fsck_strict(wt);
    let folded = git(wt, &["show", "HEAD~1:f.txt"]);
    assert!(folded.contains("line 2 edited\n"), "{folded}");
    assert!(!folded.contains("line 19 edited\n"), "{folded}");
    assert_eq!(fs::read_to_string(wt.join("f.txt")).expect("read"), edited);
    assert_eq!(git_change_id(wt, rev(wt, "HEAD~1")), derived(target));
}
